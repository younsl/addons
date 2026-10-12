# Development

Test and lint the `harbor-arm64` CLI from a local checkout.

```bash
make test       # unit tests
make coverage   # 70% line coverage gate (cargo-llvm-cov)
make lint       # rustfmt check + clippy with warnings denied
```

## Conclusion

Run make test, make coverage, and make lint before every commit. Coverage must stay at 70% or above.
