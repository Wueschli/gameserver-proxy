# Self-hosted CI runner (owner's VPS)

CI runs on GitHub-hosted `ubuntu-24.04` runners unless the repository variable
`CI_RUNNER` is set (Settings → Secrets and variables → Actions → Variables). Set
it to the label of the self-hosted runners (`gsp-ci` below) and every job except
`deploy`/`trivy` runs there; delete it to fall back to hosted runners, e.g. while
the VPS is down (a self-hosted job otherwise just queues). `CI_RUNNER_TUNNEL`
overrides the runner for the `tunnel` job alone: `ubuntu-24.04` keeps it hosted.
Why `deploy`/`trivy` always stay hosted: see the comment above `jobs:` in
`workflows/ci.yml`.

## Sizing

One runner instance runs one job at a time. A hosted runner for a private repo is
2 vCPU / 8 GB, and a full PR run starts up to five jobs at once. On a 6-core /
12 GB VPS, start with **two instances** and `CARGO_BUILD_JOBS=3` each, plus a few
GB of swap: two release-profile Rust builds side by side is what fits in 12 GB.
Expect a PR run to take longer end to end than on hosted runners (jobs queue for
a free instance). Add a third instance only if `free -h` during a full run shows
headroom.

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
apt-get install -y build-essential pkg-config protobuf-compiler python3 git curl make iproute2 util-linux
echo wireguard > /etc/modules-load.d/wireguard.conf && modprobe wireguard
# Ubuntu 24.04 restricts unprivileged user namespaces via AppArmor; the tunnel e2e needs them.
echo 'kernel.apparmor_restrict_unprivileged_userns = 0' > /etc/sysctl.d/60-gsp-ci-userns.conf
sysctl --system
```

Then check as an unprivileged user: `unshare -Urnm true && echo userns ok`.

## 3. Register the runners

GitHub: Settings → Actions → Runners → New self-hosted runner (Linux x64) shows
the current runner download URL and a registration token. Per instance `N` (1, 2):

```sh
useradd -m -s /bin/bash ghaN          # one unprivileged user per instance: own ~/.cargo, ~/.rustup
su - ghaN
mkdir actions-runner && cd actions-runner
# download + extract the tarball exactly as the GitHub page shows, then:
./config.sh --unattended --url https://github.com/Wueschli/gameserver-proxy \
  --token <TOKEN> --name vps-N --labels gsp-ci
echo 'CARGO_BUILD_JOBS=3' >> .env
exit
cd ~ghaN/actions-runner && ./svc.sh install ghaN && ./svc.sh start   # as root
```

A runner left over from the 2026-10-01 experiment can be reused by adding the
`gsp-ci` label to it in the Runners page, or removed there.

## 4. Switch CI over

Set the repository variable `CI_RUNNER=gsp-ci`, then re-run a workflow. If only
`tunnel` misbehaves on the VPS, set `CI_RUNNER_TUNNEL=ubuntu-24.04` to keep it hosted.

## Security

- Keep the repository private while it uses these runners. On a public repo, a
  fork's pull request could run arbitrary code on the VPS. Unset `CI_RUNNER` before
  ever making the repository public.
- Every branch's CI code runs as the `ghaN` users, with network access and
  whatever those users can read. Keep production services and secrets off this VPS.
- No sudo and no `docker` group for the runner users (`docker` is root-equivalent).
  The workflow's `sudo` steps are skipped (`protoc` already installed) or tolerate
  failing (`sudo -n ... || true`).
- The runner updates itself; keep the OS patched (`unattended-upgrades`).
