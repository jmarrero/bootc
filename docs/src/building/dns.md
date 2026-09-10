# DNS and `/etc/resolv.conf`

`bootc` does not configure networking or select a DNS resolver. The operating
system in the container image must provide a coherent networking and DNS
policy, just as it must provide a kernel and an init system. Usually that
policy is owned by the distribution or base image.

The recommended approach is to configure DNS through the network management
and resolver services selected by the base image. Do not treat
`/etc/resolv.conf` as a file that should be copied from the container build
environment into the booted system.

## Build-time and boot-time resolver configuration

When an image is built, the container runtime normally provides temporary
`/etc/hostname`, `/etc/hosts`, and `/etc/resolv.conf` files so that `RUN`
instructions have working networking. These files describe the build
environment. They are not the DNS configuration of the machine that will boot
the image.

When the image is installed and booted, there is no outer Podman or other
container runtime to recreate these files. The network stack included in the
image is responsible for generating the booted host's resolver configuration.
See [Container runtime vs bootc runtime](bootc-runtime.md) for more about this
distinction.

In particular:

- Do not copy the build host's `/etc/resolv.conf` into an image.
- Do not rely on the `/etc/resolv.conf` visible in a networked `RUN`
  instruction. It may be a bind mount owned by Podman or Buildah.
- Do not use an empty regular `/etc/resolv.conf` as a placeholder. `bootc`
  identifies this as a likely container-build artifact and may remove it when
  importing the image.
- Do not add files below `/run` to an image. `/run` is runtime state and is
  recreated on every boot.

If a Containerfile step must intentionally replace `/etc/resolv.conf`, run the
step with networking disabled, for example with `RUN --network=none`. This
avoids modifying a container-runtime bind mount. It should only be necessary
when deliberately changing the resolver policy inherited from the base image.

## Follow the base image's resolver policy

A derived image should normally retain its base image's DNS implementation.
Configure DNS servers, search domains, and per-interface behavior through that
implementation rather than replacing `/etc/resolv.conf`.

Common policies include:

- NetworkManager supplies per-link DNS configuration to `systemd-resolved`,
  and `/etc/resolv.conf` points to the local resolver stub.
- NetworkManager writes a regular `/etc/resolv.conf` directly.
- Another network manager or resolver owns the file under a
  distribution-specific policy.

NetworkManager and `systemd-resolved` are not alternatives with the same role.
NetworkManager configures network interfaces and may use `systemd-resolved` as
its DNS backend. The base image must ensure that its selected services, the
type and target of `/etc/resolv.conf`, and its service enablement agree.

Environment-specific DNS configuration should generally be supplied when a
machine is provisioned. If DNS configuration is intentionally lifecycle-bound
to an image, include NetworkManager connection profiles or resolver drop-ins
in the image rather than including a generated `/etc/resolv.conf`. This keeps
the configuration declarative and makes its owner clear.

## Base images using `systemd-resolved`

A base image using `systemd-resolved` should ensure that:

- `systemd-resolved` is installed and enabled.
- NetworkManager or the selected network manager is configured to supply DNS
  information to it.
- `/etc/resolv.conf` is a symlink to a resolver file generated below
  `/run/systemd/resolve`, normally
  `../run/systemd/resolve/stub-resolv.conf`.
- The symlink can be created on boot if it was not materialized during the
  container build.

The upstream `systemd-resolved` package supplies a
[`systemd-tmpfiles`](https://www.freedesktop.org/software/systemd/man/latest/tmpfiles.d.html)
rule for the last requirement:

```text
L! /etc/resolv.conf - - - - ../run/systemd/resolve/stub-resolv.conf
```

The rule establishes the symlink during boot. It does not contain DNS server
addresses; `systemd-resolved` creates the target under `/run` from runtime
network configuration. Base image authors should normally use the rule
provided by the `systemd-resolved` package. A custom distribution integration
can ship an equivalent vendor rule in `/usr/lib/tmpfiles.d`.

Do not disable `systemd-resolved` while retaining this rule and symlink. That
leaves `/etc/resolv.conf` pointing to a runtime file that no enabled service
creates.

### Fedora CoreOS example

Fedora CoreOS uses NetworkManager with `systemd-resolved`. It retains the
resolver stub symlink and configures DNS through NetworkManager connection
profiles. For example, its Butane configuration examples write NetworkManager
keyfiles under `/etc/NetworkManager/system-connections/` and set the `dns` and
`dns-search` properties there.

See the Fedora CoreOS documentation for
[host network configuration](https://docs.fedoraproject.org/en-US/fedora-coreos/sysconfig-network-configuration/)
and Fedora's description of its
[`systemd-resolved` integration](https://fedoraproject.org/wiki/Changes/systemd-resolved).
Fedora CoreOS also has an integration
[test for its resolver policy](https://github.com/coreos/fedora-coreos-config/blob/testing-devel/tests/kola/networking/resolv/systemd-resolved).

This is an example of a complete base-image policy, not a requirement that all
bootc images use `systemd-resolved`.

## Base images using a regular `/etc/resolv.conf`

A base image may instead have NetworkManager or another service write a
regular `/etc/resolv.conf`. In that case it should ensure that:

- `systemd-resolved` is not enabled.
- No tmpfiles rule recreates the `systemd-resolved` symlink.
- `/etc/resolv.conf` is absent or is a regular file that the selected service
  is configured to manage.

For Fedora-derived images, removing the `systemd-resolved` package is generally
clearer than only disabling its service: the package also owns the tmpfiles
rule that creates the symlink. If the package must remain installed, the base
image must explicitly override the vendor tmpfiles rule as well as disabling
the service.

Any Containerfile operation that removes the inherited resolver symlink should
use `RUN --network=none`, because a networked build step may see a bind mount at
`/etc/resolv.conf` instead of the image's file.

## Traditional `resolv.conf` options and caching

Selecting a resolver backend can change observable DNS behavior. Options from
traditional `resolv.conf`, including lookup retry and search behavior, do not
all have direct equivalents in `systemd-resolved`. Conversely,
`systemd-resolved` provides features such as caching and per-link split DNS
that are not provided by a regular file alone.

Base image authors should select and document a resolver policy appropriate
for their distribution. Workloads that depend on particular behavior such as
`ndots`, timeouts, attempts, caching, or split DNS should be tested against
that resolver. `bootc` does not translate options between resolver
implementations.

See the
[`systemd-resolved` documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd-resolved.html)
and NetworkManager's documentation for
[`NetworkManager.conf`](https://networkmanager.pages.freedesktop.org/NetworkManager/NetworkManager/NetworkManager.conf.html)
and
[connection settings](https://networkmanager.dev/docs/api/latest/ref-settings.html)
for the behavior and configuration supported by each component.

## Static resolver configuration

A static `/etc/resolv.conf` can be appropriate for a deliberately static
system, but it is not the default recommendation. It prevents normal dynamic
updates from DHCP, VPNs, and per-link configuration unless the network stack
is also configured not to manage the file.

If a static file is required:

1. Explicitly configure the network stack not to own `/etc/resolv.conf`.
2. Remove any inherited resolver symlink in a build step using
   `RUN --network=none`.
3. Add the intended regular file from build context; do not copy the build
   host's generated file.
4. Treat its contents as machine-local configuration when deciding whether it
   belongs in the reusable image or in provisioning.

Remember that `/etc` is persistent and uses a three-way merge across bootc
upgrades. A locally modified `/etc/resolv.conf` remains a local override and
may prevent a later image default from taking effect. See
[Filesystem: `/etc`](../filesystem.md#etc) for details.

## Application containers

Application containers and bootc hosts have different runtime behavior. By
default, Podman creates `/etc/hosts`, `/etc/hostname`, and `/etc/resolv.conf`
for each application container. Runtime options such as `podman run --dns`
control that generated file, and `--dns=none` requests use of the image's file.

The DNS options to `podman build` apply to networked `RUN` instructions; they
do not define the DNS policy of a host booted from the completed image. See the
Podman documentation for
[`podman run`](https://docs.podman.io/en/latest/markdown/podman-run.1.html) and
[`podman build`](https://docs.podman.io/en/latest/markdown/podman-build.1.html).
