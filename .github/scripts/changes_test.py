"""Tests for changes.py (changed files -> the CI areas that need to run).

Most cases run against the repo's real `cargo metadata`, so a new dependency
edge that changes what a crate affects shows up here as well as in CI. The
synthetic-graph cases pin the mechanism itself (transitive closure, ownership).

Run: python3 .github/scripts/changes_test.py   (needs cargo on PATH)
"""
import io
import os
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
sys.dont_write_bytecode = True  # no __pycache__ next to the scripts
sys.path.insert(0, HERE)

import changes  # noqa: E402

ALL = {"ui", "plugins", "tunnel", "deploy", "fuzz", "release"}


def flags(out):
    return {a for a, on in out.items() if on}


class RealRepo(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.graph = changes.load_graph(REPO)

    def areas(self, *files):
        return flags(changes.areas(list(files), self.graph))

    def test_docs_only_changes_nothing(self):
        self.assertEqual(self.areas("README.md", "docs/12-deployment.md"), set())

    def test_markdown_under_a_scoped_dir_changes_nothing(self):
        # deploy/README.md matches the `^deploy/` rule but builds and scans nothing.
        self.assertEqual(self.areas("deploy/README.md"), set())
        self.assertEqual(self.areas("crates/wayhouse-ui/web/README.md"), set())
        self.assertEqual(self.areas("deploy/README.md", "deploy/Dockerfile"), {"deploy", "release"})

    def test_empty_list_is_nothing_not_all(self):
        self.assertEqual(self.areas(), set())

    def test_ui_web_only(self):
        self.assertEqual(self.areas("crates/wayhouse-ui/web/src/App.tsx"), {"ui"})

    def test_ui_lockfile_also_runs_deploy_for_the_image_scan(self):
        self.assertEqual(
            self.areas("crates/wayhouse-ui/web/package-lock.json"),
            {"ui", "deploy", "release"},
        )

    def test_wayhouse_core_reaches_plugins_and_tunnel(self):
        self.assertEqual(
            self.areas("crates/wayhouse-core/src/pool.rs"),
            {"plugins", "tunnel", "release"},
        )

    def test_wayhouse_config_also_runs_fuzz(self):
        self.assertEqual(
            self.areas("crates/wayhouse-config/src/lib.rs"),
            {"plugins", "tunnel", "fuzz", "release"},
        )

    def test_wayhouse_http_reaches_plugins_and_tunnel(self):
        self.assertEqual(
            self.areas("crates/wayhouse-http/tests/fixtures/leaf.pem"),
            {"plugins", "tunnel", "release"},
        )

    def test_fuzz_workspace_only_runs_fuzz(self):
        self.assertEqual(
            self.areas("crates/wayhouse-config/fuzz/fuzz_targets/parse_config.rs"),
            {"fuzz"},
        )

    def test_a_plugin_crate_only_runs_plugins(self):
        self.assertEqual(self.areas("crates/plugins/a2s/src/lib.rs"), {"plugins", "release"})

    def test_plugins_workspace_root_files_run_plugins(self):
        self.assertEqual(self.areas("crates/plugins/Cargo.lock"), {"plugins", "release"})

    def test_wayhouse_agent_is_tunnel_only(self):
        self.assertEqual(self.areas("crates/wayhouse-agent/src/main.rs"), {"tunnel"})

    def test_wayhouse_ui_crate_is_tunnel_only(self):
        self.assertEqual(self.areas("crates/wayhouse-ui/src/lib.rs"), {"tunnel"})

    def test_fleet_tests_are_tunnel_only(self):
        self.assertEqual(self.areas("crates/wayhouse-fleet-tests/tests/tunnel.rs"), {"tunnel"})

    def test_wayhouse_bench_runs_no_scoped_job(self):
        self.assertEqual(self.areas("crates/wayhouse-bench/src/main.rs"), set())

    def test_wayhouse_proto_runs_plugins_and_tunnel(self):
        self.assertEqual(
            self.areas("crates/wayhouse/proto/resolver.proto"),
            {"plugins", "tunnel", "release"},
        )

    def test_an_unknown_path_under_crates_runs_every_rust_job(self):
        self.assertEqual(
            self.areas("crates/wayhouse-new/src/lib.rs"),
            {"plugins", "tunnel", "fuzz", "release"},
        )

    def test_nextest_config_runs_the_nextest_jobs(self):
        self.assertEqual(self.areas(".config/nextest.toml"), {"plugins", "tunnel", "release"})

    def test_root_cargo_lock_runs_its_workspace_and_deploy_not_fuzz(self):
        # crates/wayhouse-config/fuzz has its own workspace and lockfile.
        self.assertEqual(self.areas("Cargo.lock"), {"plugins", "tunnel", "deploy", "release"})

    def test_fuzz_lockfile_runs_fuzz_only(self):
        self.assertEqual(self.areas("crates/wayhouse-config/fuzz/Cargo.lock"), {"fuzz"})

    def test_root_manifest_runs_all_rust_and_deploy(self):
        # Inherited by every workspace's members (wayhouse-config uses workspace = true).
        self.assertEqual(
            self.areas("Cargo.toml"), {"plugins", "tunnel", "deploy", "fuzz", "release"}
        )

    def test_toolchain_runs_all_rust(self):
        self.assertEqual(
            self.areas("rust-toolchain.toml"), {"plugins", "tunnel", "fuzz", "release"}
        )

    def test_makefile_runs_tunnel_and_deploy(self):
        self.assertEqual(self.areas("Makefile"), {"tunnel", "deploy", "release"})

    def test_deploy_dir_dockerignore_and_trivyignore(self):
        for f in ("deploy/compose/wayhouse.yaml", ".dockerignore", ".trivyignore"):
            with self.subTest(f=f):
                self.assertEqual(self.areas(f), {"deploy", "release"})

    def test_workflow_edit_runs_everything(self):
        self.assertEqual(self.areas(".github/workflows/ci.yml"), ALL)

    def test_mixed_ui_web_and_deploy(self):
        self.assertEqual(
            self.areas("crates/wayhouse-ui/web/package.json", "deploy/Dockerfile"),
            {"ui", "deploy", "release"},
        )

    def test_every_package_is_loaded(self):
        names = {p.name for p in self.graph.packages.values()}
        for n in ("wayhouse", "wayhouse-agent", "wayhouse-fleet-tests", "a2s", "wayhouse-config-fuzz"):
            self.assertIn(n, names)


def pkg(name, *deps):
    return changes.Package(name, frozenset(deps))


class SyntheticGraph(unittest.TestCase):
    """The mechanism, on a graph the test controls."""

    def graph(self, **extra):
        packages = {
            "crates/wayhouse-config": pkg("wayhouse-config"),
            "crates/wayhouse-core": pkg("wayhouse-core", "crates/wayhouse-config"),
            "crates/wayhouse": pkg("wayhouse", "crates/wayhouse-core"),
            "crates/wayhouse-agent": pkg("wayhouse-agent"),
            "crates/wayhouse-controller": pkg("wayhouse-controller"),
            "crates/wayhouse-aggregator": pkg("wayhouse-aggregator"),
            "crates/wayhouse-ui": pkg("wayhouse-ui"),
            "crates/wayhouse-fleet-tests": pkg("wayhouse-fleet-tests"),
            "crates/wayhouse-config/fuzz": pkg("wayhouse-config-fuzz", "crates/wayhouse-config"),
            "crates/plugins/a2s": pkg("a2s"),
        }
        packages.update(extra)
        return changes.Graph(packages, {
            "": [d for d in packages if not d.startswith(("crates/plugins/", "crates/wayhouse-config/fuzz"))],
            "crates/plugins": ["crates/plugins/a2s"],
            "crates/wayhouse-config/fuzz": ["crates/wayhouse-config/fuzz"],
        })

    def test_a_new_dependency_edge_is_followed_transitively(self):
        # wayhouse-agent gains a dependency on wayhouse-core: a wayhouse-config change now
        # reaches it through wayhouse-core, with no hand-written list to update.
        g = self.graph(**{"crates/wayhouse-agent": pkg("wayhouse-agent", "crates/wayhouse-core")})
        self.assertIn(
            "crates/wayhouse-agent",
            changes.dependents({"crates/wayhouse-config"}, g),
        )

    def test_the_longest_owning_package_wins(self):
        g = self.graph()
        self.assertEqual(
            changes.owners("crates/wayhouse-config/fuzz/x.rs", g), {"crates/wayhouse-config/fuzz"}
        )
        self.assertEqual(changes.owners("crates/wayhouse-config/x.rs", g), {"crates/wayhouse-config"})

    def test_a_lockfile_does_not_reach_another_workspace(self):
        g = self.graph()
        got = flags(changes.areas(["Cargo.lock"], g))
        self.assertNotIn("fuzz", got)
        self.assertIn("tunnel", got)

    def test_a_crate_dir_prefix_is_not_a_partial_name_match(self):
        # crates/wayhouse-core-extra is not inside crates/wayhouse-core.
        self.assertIsNone(changes.owners("crates/wayhouse-core-extra/src/lib.rs", self.graph()))


class Main(unittest.TestCase):
    def run_main(self, argv, stdin, loader):
        out = io.StringIO()
        changes.main(argv, io.StringIO(stdin), out, io.StringIO(), loader)
        return dict(line.split("=") for line in out.getvalue().split())

    def test_all_flag_runs_everything_without_metadata(self):
        def boom(_):
            raise AssertionError("no metadata needed for --all")

        got = self.run_main(["--all"], "", boom)
        self.assertEqual({a for a, v in got.items() if v == "true"}, ALL | {"code", "docs"})

    def test_a_metadata_failure_runs_everything(self):
        def broken(_):
            raise changes.MetadataError("cargo metadata failed")

        got = self.run_main([], "crates/wayhouse-agent/src/main.rs\n", broken)
        self.assertEqual({a for a, v in got.items() if v == "true"}, ALL | {"code", "docs"})

    def test_a_roots_entry_matching_no_package_runs_everything(self):
        # e.g. wayhouse-agent renamed: a silently empty root would skip tunnel.
        got = self.run_main([], "README.md\n", lambda _: changes.Graph({}, {}))
        self.assertEqual({a for a, v in got.items() if v == "true"}, ALL | {"code", "docs"})

    def test_output_lists_every_area_once(self):
        got = self.run_main([], "README.md\n", lambda _: changes.load_graph(REPO))
        self.assertEqual(set(got), ALL | {"code", "docs"})
        self.assertEqual({a for a, v in got.items() if v == "true"}, {"docs"})

    def test_code_is_false_for_docs_only_and_true_otherwise(self):
        self.assertFalse(changes.is_code([]))
        self.assertFalse(changes.is_code(["README.md", "docs/a/b.png", "LICENSE-MIT", "crates/x/NOTES.md"]))
        self.assertTrue(changes.is_code(["README.md", "crates/wayhouse/src/main.rs"]))
        self.assertTrue(changes.is_code([".github/workflows/ci.yml"]))
        self.assertTrue(changes.is_code(["Cargo.lock"]))

    def test_docs_is_true_for_markdown_and_its_tooling(self):
        for f in ["README.md", "deploy/README.md", ".prettierrc.json", ".prettierignore", "Makefile",
                  ".github/scripts/check_md_links.py", ".github/workflows/ci.yml"]:
            self.assertTrue(changes.is_docs([f]), f)
        self.assertFalse(changes.is_docs(["crates/wayhouse/src/main.rs", "Cargo.lock"]))
        self.assertFalse(changes.is_docs([]))

    def test_docs_follows_the_all_flag_and_the_output(self):
        self.assertEqual(self.run_main(["--all"], "", None)["docs"], "true")
        got = self.run_main([], "README.md\n", lambda _: changes.load_graph(REPO))
        self.assertEqual(got["docs"], "true")
        got = self.run_main([], "crates/wayhouse/src/main.rs\n", lambda _: changes.load_graph(REPO))
        self.assertEqual(got["docs"], "false")

    def test_code_is_true_with_all_flag_and_on_metadata_failure(self):
        self.assertEqual(self.run_main(["--all"], "", None)["code"], "true")

        def broken(_):
            raise changes.MetadataError("x")

        self.assertEqual(self.run_main([], "README.md\n", broken)["code"], "true")


if __name__ == "__main__":
    unittest.main(verbosity=1)
