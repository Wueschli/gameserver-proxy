# Self-hosted CI runner (owner's VPS)

CI runs on GitHub-hosted `ubuntu-24.04` runners unless repository variables say
otherwise (Settings → Secrets and variables → Actions → Variables):

| Variable | Jobs | Set to |
|---|---|---|
| `CI_RUNNER` | build jobs: `test`, `build-release`, `plugins`, `tunnel`, `fuzz` (and the rest, unless overridden) | `gsp-ci` |
| `CI_RUNNER_LIGHT` | `changes`, `audit`, `ui` (no Rust build) | `gsp-ci-light` |
| `CI_RUNNER_DOCKER` | `deploy`, `trivy` (need Docker) | `gsp-ci-docker`, a label on exactly **one** instance |
| `CI_RUNNER_TUNNEL` | `tunnel` only | unset, or `ubuntu-24.04` to keep it hosted |

Unset or `ubuntu-24.04` means GitHub-hosted, which bills minutes. Once the free
quota is used up, hosted jobs fail without a runner. A self-hosted job just queues
while the VPS is down; delete the variables to fall back to hosted runners.
Why `CI_RUNNER_DOCKER` must be one instance: see the comment above `jobs:` in
`workflows/ci.yml`.

## Sizing

One runner instance runs one job at a time. A hosted runner for a private repo is
2 vCPU / 8 GB, and a full PR run starts up to five jobs at once. On a 6-core /
12 GB VPS, start with **two instances** and `CARGO_BUILD_JOBS=3` each, plus a few
GB of swap: two release-profile Rust builds side by side is what fits in 12 GB.
Expect a PR run to take longer end to end than on hosted runners (jobs queue for
a free instance). Add a third build instance only if `free -h` during a full run
shows headroom.

Light jobs: `changes` gates every other job, so it shouldn't wait behind a build.
Register one instance labelled `gsp-ci-light` (little CPU or RAM).

Docker jobs: one instance labelled `gsp-ci-docker`, whose user is in the `docker`
group. With prebuilt binaries `deploy` only packages images and runs the compose
smoke test (ports 9900-9903 on 127.0.0.1, `gsp` on the host network), so it is
light too.

So: four runner users on the VPS, two `gsp-ci`, one `gsp-ci-light`, one
`gsp-ci-docker`. Idle runners cost nothing.

## 1. Check the VPS (as root)

```sh
systemd-detect-virt                 # kvm/qemu/vmware/microsoft: fine; lxc/openvz: no kernel WireGuard leg
. /etc/os-release; echo "$PRETTY_NAME"
ldd --version | head -1             # glibc must be <= 2.41 (Ubuntu 24.04 = 2.39, Debian 13 = 2.41)
modprobe wireguard && lsmod | grep -w wireguard
df -h /home                         # 60-80 GB free for toolchains, caches and target dirs
```

## 2. Prepare the host once (as root)

The runner users get no sudo, so everything CI would `sudo` is done here.

```sh
apt-get update
apt-get install -y build-essential pkg-config protobuf-compiler python3 git curl make iproute2 util-linux ruby
# Docker Engine + compose plugin for the gsp-ci-docker instance: https://docs.docker.com/engine/install/
# then, weekly, drop old image layers: echo '0 4 * * 0 root docker system prune -af' > /etc/cron.d/gsp-ci-docker
echo wireguard > /etc/modules-load.d/wireguard.conf && modprobe wireguard
# Ubuntu 24.04 restricts unprivileged user namespaces via AppArmor; the tunnel e2e needs them.
echo 'kernel.apparmor_restrict_unprivileged_userns = 0' > /etc/sysctl.d/60-gsp-ci-userns.conf
sysctl --system
```

Then check as an unprivileged user: `unshare -Urnm true && echo userns ok`.

## 3. Register the runners

GitHub: Settings → Actions → Runners → New self-hosted runner (Linux x64) shows
the current runner download URL and a registration token. Per instance `N` (1-4; labels `gsp-ci`, `gsp-ci`, `gsp-ci-light`, `gsp-ci-docker`):

```sh
useradd -m -s /bin/bash ghaN          # one unprivileged user per instance: own ~/.cargo, ~/.rustup
su - ghaN
mkdir actions-runner && cd actions-runner
# download + extract the tarball exactly as the GitHub page shows, then:
./config.sh --unattended --url https://github.com/Wueschli/gameserver-proxy \
  --token <TOKEN> --name vps-N --labels gsp-ci
echo 'CARGO_BUILD_JOBS=3' >> .env
exit
# docker instance only, as root: usermod -aG docker gha4
cd ~ghaN/actions-runner && ./svc.sh install ghaN && ./svc.sh start   # as root
```

A runner left over from the 2026-10-01 experiment can be reused by adding the
`gsp-ci` label to it in the Runners page, or removed there.

## 4. Switch CI over

Set `CI_RUNNER=gsp-ci`, `CI_RUNNER_LIGHT=gsp-ci-light` and
`CI_RUNNER_DOCKER=gsp-ci-docker`, then re-run a workflow. If only `tunnel`
misbehaves on the VPS, set `CI_RUNNER_TUNNEL=ubuntu-24.04` to keep it hosted
(that needs hosted minutes).

## Security

- Keep the repository private while it uses these runners. On a public repo, a
  fork's pull request could run arbitrary code on the VPS. Unset `CI_RUNNER` before
  ever making the repository public.
- Every branch's CI code runs as the `ghaN` users, with network access and
  whatever those users can read. Keep production services and secrets off this VPS.
- No sudo for any runner user, and the `docker` group (root-equivalent) only for
  the one `gsp-ci-docker` user. Any job that lands there is effectively root on
  the VPS, which is one more reason to keep production off it.
  The workflow's `sudo` steps are skipped (`protoc` already installed) or tolerate
  failing (`sudo -n ... || true`).
- The runner updates itself; keep the OS patched (`unattended-upgrades`).
