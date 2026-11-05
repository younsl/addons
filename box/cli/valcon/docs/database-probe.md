# Database probe

How valcon reads keys and values without a network path from your machine, for operators deciding whether to allow it in a cluster.

## Path

Database queries go through a pod that already reaches the node, because that pod's network (and security group, with Security Groups for Pods) is allowed to. valcon adds one ephemeral container named `valcon-probe` (image `probe_image`, `nc` required) to that pod, then sends pipelined commands through `nc` and ends with `QUIT`. The application containers are never entered.

## Probe lifecycle

- Reused for every later query and session, and pods that already run one are chosen first
- Runs as a non-root user with a read-only root filesystem and all capabilities dropped, so restricted Pod Security admits it
- Cannot be removed (an ephemeral container limit) and stays until the pod is replaced

## Commands

Only read commands are sent: `INFO`, `SELECT`, `SCAN`, `TYPE`, `PTTL`, `MEMORY USAGE`, `STRLEN`, `GETRANGE`, `HLEN`, `HSCAN`, `LLEN`, `LRANGE`, `SCARD`, `SSCAN`, `ZCARD`, `ZRANGE`, `XLEN`, `XRANGE`. There is no free-form command input.

## Conclusion

Each target pod gets at most one hardened, reusable `valcon-probe` container that sends a fixed set of read commands, so browsing keys never writes to the cache and never enters application containers.
