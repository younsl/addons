# Limitations

Known gaps in what valcon can see or browse, for users interpreting empty or missing data.

## Known gaps

- Each refresh is a snapshot. Connections opened and closed between refreshes (short batch jobs) can be missed. Lower `--interval` and narrow with `-l` to watch them.
- Clients that connect lazily show no sockets until their first command. LAST SEEN on the clients tab keeps the last time they were connected.
- Pods without `cat` in any container show the scan error on `:pods`.
- Database browsing needs a scanned pod with a socket to the group. TLS and AUTH token clusters are not supported yet.
- Only replication groups are listed. Standalone cache clusters outside a replication group are not.

## Conclusion

Missing pods usually mean a short-lived or idle connection, or a pod without `cat`. Missing keys usually mean no connected pod, TLS, or AUTH. Check `:pods` first.
