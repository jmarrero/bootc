# New Plan: `bootc loader-config set-options-for-source`

## What Changed: ostree PR #3570 Discussion

The [ostree PR #3570](https://github.com/ostreedev/ostree/pull/3570) went
through three phases:

### Phase 1 (Feb 27): Custom BLS keys

Original implementation stored source metadata as custom BLS key-value
entries (e.g. `ostree-source-tuned nohz=full`). cgwalters reviewed and
**approved**, but raised a concern:

> "Backing up a second...I had vaguely thought we'd have 'magic comments'
> and not add new keys. I have a concern that other tools might choke on
> unknown keys."

He also suggested the naming `x-ostree-options-source-<name>`.

### Phase 2 (Feb 27 - Mar 24): Magic comments

PR was rewritten to use magic comment lines (`# x-ostree-options-source-*`).
cgwalters **approved** again on Mar 25:

> "Looks sane to me! I guess a question here is...do we actually push
> the logic for kargs here like `ostree kargs --source=foo` here and
> then rpm-ostree could reuse that?"

He also noted (inline on the comments struct):

> "In theory, this could be used outside of ostree (like bootc
> w/composefs non-sealed UKI). In fact we almost certainly should
> support it eventually there."

### Phase 3 (Mar 26, today): Pivot to bootc + BLS keys

jlebon questioned magic comments:

> "feels like it's worth doing a bit more investigation before settling
> on magic comments. FWIW, throwing AI against systemd-boot and
> rhboot/grub2 and asking 'in the BLS implementation, what happens if
> we encounter a key we don't know about', and both came back saying
> it would just be ignored."

He also suggested a colocated file in `entries/` (not `.conf`).

cgwalters responded with two ideas in quick succession:

1. **Origin file** (comment 4132229015): "since actually what we just
   need here is data lifecycle bound to the deployment, we could store
   it ~anywhere. But probably the most obvious one is the origin file.
   And if we do that, no new APIs are needed here at all right?"

2. **BLS keys + bootc** (comment 4134994436, after confirming zipl also
   ignores unknown keys): "So yeah I'm fine with going with x-ostree
   instead of magic comments." Then:

   > "Just thinking about the medium term, what I think would be nice is:
   > - Standardize `options-source-$name=`
   > - systemd `bootctl` gains e.g. `bootctl set-options-for-source
   >   --id=<id> <srcname> [options...]`
   >
   > We could head towards that place by:
   > - Using `x-options-source-$name=`
   > - Having the verb in say bootc as `bootc loader-config
   >   set-options-for-source` (easy)"

   jlebon gave a thumbs-up to this.

Then jmarrero asked about staged deployments (comment 4135322415):

> "We would still need to modify ostree no? To make sure don't drop
> these `x-options-source-$name=` entries when we do new deployments."

cgwalters confirmed (comment 4136063086):

> "Yeah, you're right definitely. [...] Right that gets to a tricky
> point which is staged deployments. Definitely those should go through
> the ostree APIs. And the staged thing is really a big wrinkle in this."

jmarrero's latest comment (4136614532, awaiting response):

> "So let's say, we revert this PR back to using keys, we call those
> keys `x-options-source-$name=` [...] But then, side step rpm-ostree
> and have `bootc loader-config set-options-for-source tuned
> mykarg=value` it calls ostree when there is a ostree backend and just
> write directly into the BLS config when composefs?"

## Current State

- cgwalters' last comment was the "big wrinkle" acknowledgment
  about staged deployments. He has NOT responded to jmarrero's
  specific proposal yet.
- The direction is clear: **BLS keys** (not comments), command in
  **bootc** (not rpm-ostree), naming `x-options-source-$name`.
- The open question is exactly how to handle the ostree staged
  deployment path.

## Architecture

```
TuneD (consumer)
  │  bootc loader-config set-options-for-source tuned nohz=full isolcpus=1-3
  ▼
bootc (orchestrator + storage)
  │  Validates source name, computes diff
  │  Directly modifies booted BLS entry on /boot
  │  Writes x-options-source-tuned key
  ▼
BLS config on /boot (persistent storage)
  x-options-source-tuned nohz=full isolcpus=1-3
```

Two concerns handled separately:

1. **Setting source kargs** (the CLI command): Directly modifies the
   booted BLS entry. No deployment staging. Works identically on
   both backends.

2. **Preserving source kargs through upgrades**: Backend-specific.
   Composefs is straightforward; ostree's staged deployment path
   is "a big wrinkle" (cgwalters' words).

## BLS Key Format

```
title Fedora Linux 42
version 6.8.0-300.fc40.x86_64
linux /vmlinuz-6.8.0-300.fc40.x86_64
initrd /initramfs-6.8.0-300.fc40.x86_64.img
options root=UUID=xxx rw nohz=full isolcpus=1-3
x-options-source-tuned nohz=full isolcpus=1-3
```

- Key: `x-options-source-<name>`
- Value: space-separated kargs owned by this source
- systemd-boot, GRUB2, and zipl all ignore unknown keys
- ostree's `OstreeBootconfigParser` already stores unknown keys in
  its hash table and writes them back (verified on `main` branch)

## CLI Design

Per cgwalters' suggestion:

```
bootc loader-config set-options-for-source <source-name> [kargs...]
```

### Examples

```bash
# Set kargs for a source
bootc loader-config set-options-for-source tuned nohz=full isolcpus=1-3

# Replace with different kargs (old auto-removed)
bootc loader-config set-options-for-source tuned nohz=on rcu_nocbs=2-7

# Clear all kargs from a source
bootc loader-config set-options-for-source tuned

# Multiple sources coexist
bootc loader-config set-options-for-source dracut rd.driver.pre=vfio-pci

# Idempotent (same kargs again)
bootc loader-config set-options-for-source tuned nohz=on rcu_nocbs=2-7
# → "No changes to kernel arguments from source 'tuned'."
```

### Validation

Source name must be non-empty and match `[a-zA-Z0-9_-]+`.

## Implementation Plan

### 1. BLS Parser — No Changes Needed

`crates/lib/src/parsers/bls_config.rs` already handles this:

- **Parsing** (line 254): Unknown keys go into
  `extra: HashMap<String, String>`. A line like
  `x-options-source-tuned nohz=full` is stored as
  `extra["x-options-source-tuned"] = "nohz=full"`.
- **Writing** (line 136-138): The `Display` impl writes all `extra`
  entries as `{key} {value}` lines.
- **Roundtrip**: Parse → Display → Parse preserves unknown keys.

The only minor consideration is that `HashMap` doesn't preserve
insertion order (keys may be reordered). This is fine — BLS key
order is not significant.

### 2. New CLI Subcommand

In `crates/lib/src/cli.rs`, add to the `Opt` enum:

```rust
/// Manage boot loader configuration.
#[clap(subcommand)]
LoaderConfig(LoaderConfigOpts),
```

With:

```rust
#[derive(Debug, clap::Subcommand, PartialEq, Eq)]
pub(crate) enum LoaderConfigOpts {
    /// Set kernel arguments owned by a named source.
    ///
    /// When a source is specified, all previous kargs from that source
    /// are removed from the boot entry and replaced with the new set.
    /// Source ownership is tracked via x-options-source-<name> keys
    /// in the BLS config file on /boot.
    SetOptionsForSource {
        /// The source name (e.g. "tuned", "dracut").
        source: String,
        /// Kernel arguments for this source. If empty, clears all
        /// kargs from this source.
        #[clap(trailing_var_arg = true)]
        kargs: Vec<String>,
    },
}
```

### 3. Core Logic Module

Create `crates/lib/src/loader_config.rs`:

#### Constants and helpers

```rust
const OPTIONS_SOURCE_PREFIX: &str = "x-options-source-";

fn source_key(source: &str) -> String {
    format!("{OPTIONS_SOURCE_PREFIX}{source}")
}

fn validate_source_name(source: &str) -> Result<()> {
    // non-empty, [a-zA-Z0-9_-]+
}
```

#### Diff computation

```rust
fn compute_source_diff(
    old_source_kargs: Option<&str>,
    new_source_kargs: &[String],
) -> (Vec<String>, Vec<String>)  // (to_add, to_remove)
```

Set-based diff. Handles: no-op, add-only, remove-only, replace, clear.

#### Main entry point

```rust
pub(crate) fn set_options_for_source(
    source: &str,
    new_kargs: &[String],
) -> Result<()>
```

Steps:
1. Validate source name
2. Find and parse the booted BLS entry from `/boot/loader/entries/`
3. Read current source kargs from `extra["x-options-source-<name>"]`
4. Compute diff
5. If no changes, print message and return
6. Modify the `options` line (remove old, add new)
7. Update the `x-options-source-<name>` key (or remove if clearing)
8. Write the modified BLS config back to disk (atomic rename)

#### Finding the booted BLS entry

Both backends write BLS configs to `/boot/loader/entries/*.conf`.
The booted entry can be identified by matching `/proc/cmdline`
against the `options` field in each BLS entry. Alternatively:

- **Composefs**: Match `composefs=<digest>` in options against
  the booted verity digest from `/proc/cmdline`
- **ostree**: Match `ostree=` parameter

The simplest approach: iterate BLS entries, parse each, find the
one whose `options` field matches `/proc/cmdline`.

#### Writing the BLS file

`/boot` is typically mounted read-only. The command will need to
remount it read-write (similar to what the existing kargs docs
suggest: `unshare -m; mount -o remount,rw /boot`). This is a
direct modification — no deployment staging, no reboot required
for the metadata change (though the kargs themselves take effect
on next boot).

### 4. Preservation Through Upgrades

#### Composefs backend — straightforward

In `crates/lib/src/bootc_composefs/boot.rs`,
`setup_composefs_bls_boot()` upgrade path (line 544-582):

Currently reads the booted BLS config, extracts only `options`,
builds a new `BLSConfig` from scratch. The source kargs are in
`options` so they survive, but the `x-options-source-*` ownership
tracking keys are lost (they're in `extra`, which isn't carried
forward).

**Fix**: After building the new `BLSConfig` (around line 673-688),
copy `x-options-source-*` entries from the booted config's `extra`:

```rust
for (key, value) in &current_cfg.extra {
    if key.starts_with(OPTIONS_SOURCE_PREFIX) {
        bls_config.extra.insert(key.clone(), value.clone());
    }
}
```

The kargs themselves are already preserved because the upgrade
path copies all cmdline args from the booted entry (lines 550-555).

#### ostree backend — the "big wrinkle"

When bootc upgrades via the ostree backend:

1. bootc calls `ostree_sysroot_stage_tree_with_options()` with
   `override_kernel_argv` (the kargs as a string array)
2. ostree serializes only `target`, `merge-deployment`, `kargs`,
   and `overlay-initrds` to the staged GVariant at
   `/run/ostree/staged-deployment`
3. At shutdown, `_ostree_sysroot_reload_staged()` creates a fresh
   `OstreeBootconfigParser` from the kargs alone
4. `install_deployment_kernel()` writes the BLS file from this
   parser — **all extra keys are lost**

The source-tracked kargs ARE preserved in the `options` line (step
1 passes them as part of `override_kernel_argv`). Only the ownership
metadata (`x-options-source-*` keys) is lost.

**Verified**: ostree's `OstreeBootconfigParser` on `main` already
preserves unknown keys through parse/write roundtrips (they go into
the hash table and get written back). The gap is specifically in the
staged deployment GVariant serialization, which only stores `kargs`
(the options string array) and discards everything else from the
bootconfig.

**Required ostree change**: Preserve unknown BLS keys through the
staging roundtrip. Specifically:

1. In `stage_tree_with_options()`: After existing serialization,
   read the merge deployment's BLS config, extract unknown keys
   (not in the standard set: title, version, linux, initrd, options,
   machine-id, sort-key, devicetree, fdtdir, aboot, abootcfg),
   serialize them as `"bootconfig-extra-keys"` (type `a{ss}`) in
   the staged GVariant.

2. In `_ostree_sysroot_reload_staged()`: Read `"bootconfig-extra-keys"`
   from the GVariant, set them on the bootconfig parser via
   `ostree_bootconfig_parser_set()`.

3. `install_deployment_kernel()` already writes all keys from the
   hash table — no change needed here.

This is much simpler than the current ostree PR:
- No magic comment parsing/writing/allowlist
- No new public API (`get_comment`, `set_comment`)
- No `GPtrArray *comments` field
- Just ~30 lines: serialize/deserialize extra keys in 2 functions

cgwalters said this "should go through the ostree APIs" and called
it "a big wrinkle," so this ostree change is expected to be needed.

**Degraded behavior without the ostree change**: The kargs
themselves persist through upgrades (they're in `override_kernel_argv`).
Only the `x-options-source-*` ownership tracking is lost. This means
after an ostree-backend upgrade, source idempotency breaks (the next
`set-options-for-source` call won't find previous source kargs to
diff against). However, the command still works — it just adds rather
than replaces.

### 5. Testing

Per REVIEW.md: table-driven tests, separate parsing from I/O.

#### Unit tests (in `loader_config.rs`)

| Test | What it verifies |
|------|-----------------|
| Source name validation | Valid names, empty, special chars, Unicode |
| Diff: no changes | Same kargs → empty add/remove |
| Diff: add only | New source → all kargs added |
| Diff: remove only | Clear source → all kargs removed |
| Diff: replace | Different kargs → correct add/remove sets |
| BLS roundtrip with extra keys | Parse with `x-options-source-*`, modify, write, re-parse |

#### Integration tests

| Test | What it verifies |
|------|-----------------|
| Basic append | Source kargs added to options + key written |
| Replacement | Old source kargs removed, new added |
| Clear | Source kargs removed, key removed/empty |
| Idempotency | Same kargs twice → no changes |
| Multiple sources | Independent sources coexist |
| Invalid source name | Rejected with error |

### 6. Documentation

Update `docs/src/building/kernel-arguments.md`:
- Remove the "bootc does not itself offer an API" caveat
- Add section for `bootc loader-config set-options-for-source`

## Files to Create/Modify

### bootc (this repo)

| File | Action | Description |
|------|--------|-------------|
| `crates/lib/src/loader_config.rs` | Create | Core logic: validation, diff, BLS read/write |
| `crates/lib/src/cli.rs` | Modify | Add `LoaderConfig` to `Opt` enum, dispatch |
| `crates/lib/src/lib.rs` | Modify | Add `mod loader_config;` |
| `crates/lib/src/bootc_composefs/boot.rs` | Modify | Preserve `x-options-source-*` during upgrade |
| `docs/src/building/kernel-arguments.md` | Modify | Document the new command |

### ostree (separate PR, simplified)

| File | Action | Description |
|------|--------|-------------|
| `src/libostree/ostree-sysroot-deploy.c` | Modify | Serialize extra bootconfig keys in staged GVariant |
| `src/libostree/ostree-sysroot.c` | Modify | Deserialize extra bootconfig keys from staged GVariant |

~30 lines total, versus 548 lines / 9 files in the current PR.

### TuneD (separate PR, updated)

Update the `kargs-source` branch to call
`bootc loader-config set-options-for-source` instead of
`rpm-ostree kargs --source=tuned`. Detection changes from checking
`rpm-ostree kargs --help` to checking for `bootc` availability.

### rpm-ostree

No changes needed. The `source-kargs` branch work is no longer
required.

## Commit Organization

Per REVIEW.md: atomic commits, preparatory refactoring separate
from behavior changes.

1. **loader-config: Add source kargs diff logic and unit tests** —
   Pure functions, no I/O. `validate_source_name()`,
   `compute_source_diff()`, table-driven tests.

2. **loader-config: Add `bootc loader-config set-options-for-source`** —
   CLI wiring, BLS read/write, end-to-end flow. Integration tests.

3. **composefs: Preserve x-options-source keys during upgrades** —
   Change to `setup_composefs_bls_boot()` in boot.rs.

4. **docs: Document bootc loader-config set-options-for-source** —
   Update kernel-arguments.md.

## Open Questions

### 1. Storage: BLS keys vs origin file

cgwalters mentioned the origin file as an alternative (comment
4132229015): "the most obvious one is the origin file. And if we do
that, no new APIs are needed here at all."

But then in comment 4134994436, he proposed `x-options-source-$name=`
as BLS keys with `bootc loader-config set-options-for-source`. These
are different storage mechanisms. The BLS key approach is what jlebon
thumbs-upped and what cgwalters' final detailed proposal describes.

**Plan proceeds with BLS keys** as the storage format, which matches
the latest and most specific proposal. The origin file idea seems to
have been a thought-in-progress that was superseded.

### 2. Staged deployments

cgwalters called this "a big wrinkle" and said they "should go through
the ostree APIs." This plan proposes a simplified ostree change (~30
lines) to serialize/deserialize extra BLS keys through the staged
GVariant. This is the same mechanism as the current PR, just much
simpler because:
- No comment parsing — regular key-value preservation
- No new public API needed
- No allowlist — preserve all unknown keys (or prefix-filter for
  `x-options-source-`)

### 3. cgwalters hasn't responded to jmarrero's latest comment

The latest comment (4136614532) asks about the exact approach but has
no response yet. The plan is based on the most specific proposal
cgwalters made (comment 4134994436) plus his acknowledgment that
ostree changes are needed for staging (comment 4136063086).

### 4. Naming: `x-options-source-$name` vs `x-ostree-options-source-$name`

cgwalters used both at different times:
- Comment 4134929832: "fine with going with x-ostree instead of magic
  comments"
- Comment 4134994436: "Using `x-options-source-$name=`"

The latter (without `ostree` in the name) is the more recent and
intentional one, aligning with the standardization direction toward
`options-source-$name=` in bootctl. **Plan uses `x-options-source-`**.

### 5. What about `--id=<id>` from the bootctl proposal?

cgwalters' bootctl proposal included `--id=<id>` to target a specific
boot entry. For the bootc command, this could be a future addition.
The initial implementation targets the booted deployment's BLS entry.

## Comparison to Original Design

| Aspect | Original (ostree + rpm-ostree) | New (bootc) |
|--------|-------------------------------|-------------|
| Storage | Magic comments (`# x-ostree-options-source-*`) | BLS keys (`x-options-source-*`) |
| Command | `rpm-ostree kargs --source=tuned` | `bootc loader-config set-options-for-source tuned` |
| Repos changed | 3 (ostree 548 LOC, rpm-ostree 586 LOC, TuneD 339 LOC) | 2 (bootc, TuneD) + small ostree fix (~30 LOC) |
| Mechanism | Deployment staging (reboot to apply) | Direct BLS modification (no deployment) |
| composefs support | N/A | Native |
| Standardization | ostree-specific | Toward bootctl standardization |

## Related Links

- ostree PR: https://github.com/ostreedev/ostree/pull/3570
- bootc issue: https://github.com/bootc-dev/bootc/issues/899
- TuneD PR: https://github.com/redhat-performance/tuned/pull/821
- cgwalters' bootc proposal: https://github.com/ostreedev/ostree/pull/3570#issuecomment-4134994436
- Original context doc: ~/kargs-source.md
