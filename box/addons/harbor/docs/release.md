# Release

How a new Harbor version reaches GHCR.

Bumping [VERSION](../VERSION) on `main` triggers [release-harbor-arm64.yml](../../../../.github/workflows/release-harbor-arm64.yml). It builds on an `ubuntu-26.04-arm` runner and skips when every image already has the tag on GHCR. To rebuild a published version, dispatch the workflow with `force=true`.

## Conclusion

Bump VERSION to release. Dispatch with force=true to rebuild an existing version.
