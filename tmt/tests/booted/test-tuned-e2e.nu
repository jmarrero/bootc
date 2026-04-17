# number: 43
# tmt:
#   summary: E2E test for TuneD + bootc + transient /etc
#   duration: 30m
#
# This test verifies the full stack: TuneD uses bootc loader-entries
# set-options-for-source to manage kernel arguments on a system with
# transient /etc. It covers:
#
# 1. Transient /etc is active (files in /etc are lost on reboot)
# 2. TuneD applies a profile with kernel args via bootc source tracking
# 3. Kargs survive reboot and BLS source key is present
# 4. Idempotency: re-applying the same profile after reboot (with wiped
#    /etc/tuned/bootcmdline) does NOT stage a new deployment
# 5. Profile switch correctly removes old kargs
#
# This test requires a custom image with TuneD + transient /etc.
# Build with: just build-tuned-e2e
# Run with:   just test-tmt-tuned-e2e
#
# Requires: bootc with loader-entries set-options-for-source,
#           ostree >= 2026.1, TuneD with bootc support
# See: https://github.com/bootc-dev/bootc/issues/899
use std assert
use tap.nu

def parse_cmdline [] {
    open /proc/cmdline | str trim | split row " "
}

# Read x-options-source-* keys from the booted BLS entry.
def read_bls_source_keys [] {
    let entries = glob /boot/loader/entries/ostree-*.conf | sort
    if ($entries | length) == 0 {
        error make { msg: "No BLS entries found" }
    }
    let entry = open ($entries | last)
    $entry | lines | where { |line| $line starts-with "x-options-source-" }
}

def first_boot [] {
    tap begin "tuned-e2e: TuneD + bootc + transient /etc"

    # -- Verify setup --

    # Transient /etc must be enabled
    let prc = open /usr/lib/ostree/prepare-root.conf
    assert ($prc | str contains "transient=true") "transient /etc must be enabled"
    print "ok: transient /etc is configured"

    # bootc set-options-for-source must be available
    let r = do -i { bootc loader-entries set-options-for-source --help } | complete
    assert ($r.exit_code == 0) "bootc set-options-for-source must be available"
    print "ok: bootc set-options-for-source is available"

    # Create a marker file to verify transient /etc on next boot
    "transient-test-marker" | save /etc/tuned/test-transient

    # -- Apply TuneD profile with kernel args --
    systemctl start tuned
    tuned-adm profile network-latency

    # Verify deployment is staged
    let st = bootc status --json | from json
    assert ($st.status.staged != null) "deployment should be staged after tuned-adm profile"
    print "ok: TuneD staged a deployment via bootc"

    tmt-reboot
}

def second_boot [] {
    # -- Verify transient /etc --
    let marker_exists = ("/etc/tuned/test-transient" | path exists)
    assert (not $marker_exists) "transient /etc should have wiped marker file"
    print "ok: transient /etc is working (marker file gone)"

    # Verify bootcmdline was wiped by transient /etc
    let bootcmdline = open /etc/tuned/bootcmdline
    let tuned_var = $bootcmdline | lines | where { |l| $l starts-with "TUNED_BOOT_CMDLINE=" }
    if ($tuned_var | length) > 0 {
        let val = $tuned_var | first | split row "=" | skip 1 | str join "="
        # The value should be empty (wiped) or contain the default empty quotes
        assert ($val == "" or $val == "\"\"") "TUNED_BOOT_CMDLINE should be empty after transient /etc wipe"
    }
    print "ok: bootcmdline was wiped by transient /etc"

    # -- Verify kargs survived --
    let cmdline = parse_cmdline
    assert ("skew_tick=1" in $cmdline) "skew_tick=1 should be in cmdline"
    assert ("tsc=reliable" in $cmdline) "tsc=reliable should be in cmdline"
    assert ("rcupdate.rcu_normal_after_boot=1" in $cmdline) "rcupdate karg should be in cmdline"
    print "ok: network-latency kargs survived reboot"

    # -- Verify BLS source key --
    let source_keys = read_bls_source_keys
    let tuned_keys = $source_keys | where { |k| $k starts-with "x-options-source-tuned" }
    assert (($tuned_keys | length) > 0) "x-options-source-tuned BLS key should exist"
    let tuned_key = $tuned_keys | first
    assert ($tuned_key | str contains "skew_tick=1") "BLS key should contain skew_tick=1"
    print "ok: BLS source key x-options-source-tuned present"

    # -- THE MONEY TEST: re-apply same profile (must be idempotent) --
    systemctl start tuned
    tuned-adm profile network-latency

    # Verify NO deployment was staged (kargs already correct)
    let st = bootc status --json | from json
    assert ($st.status.staged == null) "no deployment should be staged (idempotent)"
    print "ok: IDEMPOTENCY - re-applying same profile did NOT stage deployment"

    # -- Profile switch: change to throughput-performance (no kargs) --
    tuned-adm profile throughput-performance

    # Verify deployment IS staged (kargs changed)
    let st = bootc status --json | from json
    assert ($st.status.staged != null) "deployment should be staged after profile switch"
    print "ok: profile switch staged a new deployment"

    tmt-reboot
}

def third_boot [] {
    # -- Verify kargs removed after profile switch --
    let cmdline = parse_cmdline
    assert ("skew_tick=1" not-in $cmdline) "skew_tick=1 should NOT be in cmdline after switch"
    assert ("tsc=reliable" not-in $cmdline) "tsc=reliable should NOT be in cmdline after switch"
    print "ok: network-latency kargs removed after profile switch"

    # BLS source key should be tombstoned (empty value)
    let source_keys = read_bls_source_keys
    let tuned_keys = $source_keys | where { |k| $k starts-with "x-options-source-tuned" }
    assert (($tuned_keys | length) > 0) "x-options-source-tuned BLS key should still exist (tombstone)"
    let tuned_key = $tuned_keys | first
    assert (not ($tuned_key | str contains "skew_tick=1")) "tombstoned BLS key should NOT contain kargs"
    print "ok: BLS source key tombstoned after profile switch"

    tap ok
}

def main [] {
    match $env.TMT_REBOOT_COUNT? {
        null | "0" => first_boot,
        "1" => second_boot,
        "2" => third_boot,
        $o => { error make { msg: $"Unexpected TMT_REBOOT_COUNT ($o)" } },
    }
}
