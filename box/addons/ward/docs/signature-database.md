# Signature database

How the signed signature feed is built, verified, rolled out to nodes, and stored on each agent. The rollout design is in [Design: signature rollout](designs/signature-rollout.md) and the on-node format in [Design: detection engine](designs/detection-engine.md).

## Build

The [ward-db workflow](../../../../.github/workflows/ward-db.yml) rebuilds the database daily:

1. Downloads the [MalwareBazaar](https://bazaar.abuse.ch/) SHA-256 dump and the [YARA Forge](https://github.com/YARAHQ/yara-forge) core rule set, and takes the repository [allowlist](../feed/allowlist.txt).
2. Exports a known-good corpus from popular base and runtime images for both `linux/amd64` and `linux/arm64` (listed in [ci-ward-db.sh](../../../../.github/scripts/ci-ward-db.sh)).
3. Packs everything with `ward feed build` into `ward-db.json.gz` (gzipped JSON: manifest, signatures, hash sets, YARA sources, disabled YARA rules) and prunes it against the allowlist and the corpus (see [Pruning](#pruning)).
4. Runs `ward feed check`: the database must match the EICAR self-test and produce zero hits on the corpus.
5. Signs the file with `cosign sign-blob` (ECDSA P-256 over the gzipped bytes) in a separate job that alone holds the signing key.
6. Pushes it with oras to `ghcr.io/younsl/ward-db:<revision>` and moves `latest`.

A run whose inputs (feed sources and allowlist) match the published `latest` publishes nothing.

## Pruning

MalwareBazaar accepts submissions from anyone, so a benign binary can end up in the feed. In `Protect` mode one such hash kills that workload on every node, so `ward feed build` removes known-good content before the database ships:

| Match | Action |
| --- | --- |
| Feed hash listed in an `--allowlist` file | Removed from the hash sets and signature hash lists |
| Feed hash of a corpus file | Removed the same way, counted as known-good |
| YARA rule that matches a corpus file | Added to the database's disabled rule list, which every engine skips |
| Feed signature pattern that matches a corpus file | Signature removed |

- Each distinct corpus file is scanned once, so identical files across images and architectures cost one scan.
- The build fails when pruning would disable more YARA rules and signatures than `--max-disabled` (default 25). That many hits means a broken rule set, not a few noisy rules.
- The build also fails when a corpus file matches content it cannot prune, such as a built-in signature.
- The build log lists every disabled rule with the corpus file it matched.

To exclude a hash the corpus does not cover, add it to [feed/allowlist.txt](../feed/allowlist.txt). It takes effect in the next daily build. Clusters that cannot wait use `spec.indicators.allow`.

## Artifact format

| Item | Value |
| --- | --- |
| Artifact type | `application/vnd.ward.db.v1` |
| Layer 1 | `ward-db.json.gz`, `application/vnd.ward.db.v1+gzip` |
| Layer 2 | `ward-db.json.gz.sig`, `application/vnd.ward.db.signature.v1`, DER or base64 of DER |

Build your own feed with the same layout and point `spec.feed.image` and `spec.feed.publicKey` at it.

## Verification

Before any agent sees a revision, the operator resolves the tag to a digest, downloads both layers, and checks the layer digest, the signature against `spec.feed.publicKey`, and that the content compiles into a detection engine. A revision that fails any check never ships. `insecureSkipVerify: true` skips only the signature check and is for development.

Agents check the layer digest again on download, whether from the manager or the registry.

## Ring rollout

A new revision does not reach every node at once.

1. The canary ring runs it first: nodes labeled `ward.sh/ring=canary` plus `rollout.canaryPercent` of the rest, chosen by a hash of the node name so the ring is stable.
2. After `rollout.soak` the revision is promoted to every node and becomes stable.
3. The rollout halts instead when a canary agent fails to load the revision, when the canary ring's detection rate per agent exceeds the broad ring's concurrent rate times `rollout.maxDetectionRatio` (counts below `rollout.minDetections` never halt), or when canary agents do not converge within three soak periods.

A halted revision is never retried, the canary ring returns to the stable revision, and `status.content.phase` shows `Halted` with the reason. The next published revision starts a fresh rollout. Pin a revision with an `@sha256:` reference, roll back by pinning the previous digest.

Agents report their active revision and detection counters in their Lease `ward-<node>`, which is all the operator needs to evaluate the gates.

## On the node

- Feed hashes are stored as a sorted index of fixed-size records on the node with an in-memory bloom filter in front. A million hashes cost the agent about 2 MiB of memory, and clean files rarely touch the disk. There is no remote lookup.
- A new revision loads beside the running one and must catch the EICAR self-test before it swaps in. The swap happens in place, no Pod restarts.
- The agent keeps the last known good revision on the node. A revision that fails to load leaves the previous one running, and an agent that restarts without the manager or the registry starts from it.

## Conclusion

Known-good content is pruned at build time, verification happens before distribution, rollout happens in rings with automatic halts, and every node always has a working revision. Custom content that should not wait for the feed goes into a [WardSignature](configuration.md#wardsignature).
