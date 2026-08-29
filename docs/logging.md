# URMA logging: where the logs are and how to read them

urma-rs itself carries no logging code. The URMA user-space libraries
(`liburma.so` core + providers such as `libuvs.so`/tpsa) log to **syslog** by
default, so all diagnosis starts from the system log. This document explains
the mechanism and the exact commands. Source references are the vendored
headers under `include/` and the umdk implementation
(`umdk/src/urma/lib/urma/core/urma_log.c`, `umdk/src/urma/lib/uvs/core/tpsa_log.c`).

## How the URMA libraries log

- Both libraries call `openlog(NULL, LOG_PID | LOG_CONS | LOG_NDELAY,
  LOG_USER)` from an `.so` constructor — i.e. at process load, before `main`.
  Messages therefore go to syslog **facility `user`**, tagged with the
  process name and PID, with no application code involved.
- Message format (grep anchor is the literal `[URMA]` prefix):

  ```
  [URMA][file:function:line][tid][thread_tag|process_name][liburma|libuvs] message
  ```

- The two components have **independent** log levels:

  | Component          | Env var          | Default level |
  |--------------------|------------------|---------------|
  | liburma core       | `URMA_LOG_LEVEL` | `info`        |
  | libuvs (tpsa provider) | `UVS_LOG_LEVEL` | `info`    |

  Values are **strings**, case-insensitive: `fatal` | `error` | `warning` |
  `info` | `debug` (mapping to `URMA_VLOG_LEVEL_*` 2/3/4/6/7). Unset or
  invalid values keep the default. The env var is read once in the library
  constructor, so it must be set **before the process starts** — it cannot be
  changed at runtime from outside.
- At the default `info` level, `error`/`fatal` messages are already emitted —
  basic failure diagnosis needs **no** configuration. `debug` is only needed
  for import-path detail (which port pair failed, ioctl errno, topology
  lookups).
- Hot log points are rate-limited (a fixed quota per time window); suppressed
  messages are accounted with a `rate limit: N logs suppressed ...` summary
  line instead of flooding the log.

## Reading the logs

openEuler defaults to systemd-journald (usually with rsyslog forwarding). Pick
whichever your system runs.

**journald:**

```bash
# follow all URMA user-space logs live (works for both liburma and libuvs)
journalctl -f | grep --line-buffered '\[URMA\]'

# only your process (syslog ident = process name), e.g. urma_hello
journalctl -t urma_hello -f

# only errors and above from a specific boot/run
journalctl -b -p err | grep '\[URMA\]'
```

**rsyslog** (classic text files; on openEuler/CentOS `user.*` lands in
`/var/log/messages`):

```bash
tail -f /var/log/messages | grep --line-buffered '\[URMA\]'
grep '\[URMA\]' /var/log/messages | less
```

**Full-verbosity run** (the usual first step when an import/register fails on
a real device):

```bash
URMA_LOG_LEVEL=debug UVS_LOG_LEVEL=debug \
    cargo run --example urma_hello -- -d bonding_dev_0 -i <peer_ip>
# then, in another terminal:
journalctl -f | grep --line-buffered '\[URMA\]'
```

For two-node runs, collect on **both** machines — the import exchange
involves both kernels and both user-space stacks.

## Kernel-side logs

The user-space libraries are only half the story. Device/bonding exchange
failures (e.g. `ubagg_connect_xchg_seg` / `ubagg_session_timeout`, or
`UDMA: tp mode is not supported`) are logged by the kernel modules, not by
syslog from user space:

```bash
dmesg | grep -iE 'ubagg|udma|urma'
# or live:
dmesg -w | grep -iE 'ubagg|udma|urma'
```

## Environments without a syslog daemon (containers, minimal images)

With no journald/rsyslog running, syslog messages are effectively lost
(`LOG_CONS` only falls back to the system console on send failure). The
escape hatch is the C API `urma_register_log_func` /
`urma_register_loc_log_func` (bindings exist in `urma_rs::ffi`), which lets an
application install its own sink. Note this redirects **liburma core** logs
only: the tpsa provider (`libuvs`) always writes to syslog directly and
ignores the registered callback, so the system log remains the only complete
source for provider-level diagnosis.
