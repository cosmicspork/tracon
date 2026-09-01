# Changelog

## [0.10.0](https://github.com/cosmicspork/tracon/compare/v0.9.1...v0.10.0) (2026-09-01)


### Features

* **wrapper:** add AppImage self-update ([#101](https://github.com/cosmicspork/tracon/issues/101)) ([6ea02d4](https://github.com/cosmicspork/tracon/commit/6ea02d4bf07493d721ddaadba6f1ed91c892e853))

## [0.9.1](https://github.com/cosmicspork/tracon/compare/v0.9.0...v0.9.1) (2026-09-01)


### Bug Fixes

* **wrapper:** a menu-only tray icon, a ⌘Q that listens, and one instance ([#99](https://github.com/cosmicspork/tracon/issues/99)) ([79da7f6](https://github.com/cosmicspork/tracon/commit/79da7f6930d896ebf2ea53b3de91767059c6b875))

## [0.9.0](https://github.com/cosmicspork/tracon/compare/v0.8.0...v0.9.0) (2026-08-31)


### Features

* **wrapper:** open the window at launch, and say what ⌘Q and the dock do ([#97](https://github.com/cosmicspork/tracon/issues/97)) ([34e6142](https://github.com/cosmicspork/tracon/commit/34e61427a2ba51fc928e328b0976aaa3361b6952))


### Bug Fixes

* **spa:** keep the rail's footer at the bottom of the rail ([#96](https://github.com/cosmicspork/tracon/issues/96)) ([3d0c047](https://github.com/cosmicspork/tracon/commit/3d0c047161fe30a69df79d26c48a460f5befa673))

## [0.8.0](https://github.com/cosmicspork/tracon/compare/v0.7.0...v0.8.0) (2026-08-31)


### Features

* a settings pane, so a node is stood up without a shell on it ([#94](https://github.com/cosmicspork/tracon/issues/94)) ([b29e15f](https://github.com/cosmicspork/tracon/commit/b29e15fda70e8047fd353c18a9fd9978fe134c6f))


### Bug Fixes

* resolve the podman binary without a login-shell PATH ([#90](https://github.com/cosmicspork/tracon/issues/90)) ([0a1424c](https://github.com/cosmicspork/tracon/commit/0a1424c35efd24c37d62503332f6cea2055f438e))
* **spa:** coarser ages, honest session rows, an unclipped rail ([#93](https://github.com/cosmicspork/tracon/issues/93)) ([d7a6878](https://github.com/cosmicspork/tracon/commit/d7a687853ee84d9b6ac21d5c9f5ca3f3556e3874))
* **wrapper:** one template tray icon, and a dock icon that opens the window ([#91](https://github.com/cosmicspork/tracon/issues/91)) ([393a387](https://github.com/cosmicspork/tracon/commit/393a38738132de1c172dadd81f8c36aa3ddf5729))

## [0.7.0](https://github.com/cosmicspork/tracon/compare/v0.6.0...v0.7.0) (2026-08-31)


### ⚠ BREAKING CHANGES

* CONTRACT_VERSION 2 -> 3. A node on this release drops frames from, and has its frames dropped by, any peer still on 0.6.0; the hub and every node must be upgraded together.

### Features

* anywhere operations — phone-first provisioning, forge repos, and mesh provider control ([#87](https://github.com/cosmicspork/tracon/issues/87)) ([72c28f3](https://github.com/cosmicspork/tracon/commit/72c28f37f1f8f2efa764151088e2783addabfaf6))

## [0.6.0](https://github.com/cosmicspork/tracon/compare/v0.5.0...v0.6.0) (2026-08-29)


### Features

* **adapter:** drive Claude Code over its stream-json control protocol ([#74](https://github.com/cosmicspork/tracon/issues/74)) ([fbeb4d8](https://github.com/cosmicspork/tracon/commit/fbeb4d896f15f76c705e177399eec36f6d8ab59b))
* **corpus:** fuse the vector index into recall ([#75](https://github.com/cosmicspork/tracon/issues/75)) ([4fa9c67](https://github.com/cosmicspork/tracon/commit/4fa9c6792c6f979f8e003bd5d59892428a6683b5))
* embeddings and a vector index beside FTS5 ([#71](https://github.com/cosmicspork/tracon/issues/71)) ([f11ce87](https://github.com/cosmicspork/tracon/commit/f11ce872e3697f309cae84b2f43115c03cf6b9f8))
* **embed:** let the embedding endpoint want an API key ([#77](https://github.com/cosmicspork/tracon/issues/77)) ([816339e](https://github.com/cosmicspork/tracon/commit/816339e85580f127023778c603fce3db76851b29))
* **notify:** push to the phone from the node itself, no bridge in between ([#82](https://github.com/cosmicspork/tracon/issues/82)) ([f774511](https://github.com/cosmicspork/tracon/commit/f7745117ace87f8550b84e1e71ee29a088a74f04))
* **wrapper:** let the app run the node it talks to ([#78](https://github.com/cosmicspork/tracon/issues/78)) ([58cbda8](https://github.com/cosmicspork/tracon/commit/58cbda827607910705ce0122f29bd542e7a25828))


### Bug Fixes

* **config:** stop the test suite writing the operator's state directory ([#79](https://github.com/cosmicspork/tracon/issues/79)) ([9fd19b1](https://github.com/cosmicspork/tracon/commit/9fd19b1ab5a76e4208e823ed0cbb2a0e58fde9f5))
* **spa:** stop the patch fuzz test spawning a repo per round ([#72](https://github.com/cosmicspork/tracon/issues/72)) ([48d55ef](https://github.com/cosmicspork/tracon/commit/48d55ef21394a7574d92a33c95f731b3f3bf3c2a))

## [0.5.0](https://github.com/cosmicspork/tracon/compare/v0.4.0...v0.5.0) (2026-08-28)


### Features

* edit a reviewed diff and send it back as a patch ([#66](https://github.com/cosmicspork/tracon/issues/66)) ([7b405f0](https://github.com/cosmicspork/tracon/commit/7b405f010c189221859d8df2682fcb82fe2382a4))
* **node:** install the node under systemd or launchd ([#67](https://github.com/cosmicspork/tracon/issues/67)) ([b1f2658](https://github.com/cosmicspork/tracon/commit/b1f2658534448c29758f9dd9c7eb996f57eed12f))
* **node:** operator token and cookie auth for off-machine clients ([#62](https://github.com/cosmicspork/tracon/issues/62)) ([1a2cc01](https://github.com/cosmicspork/tracon/commit/1a2cc01c5a27ffe7b1626d7063f3057266e44943))
* **node:** push what waits on the operator to a channel's sink ([#64](https://github.com/cosmicspork/tracon/issues/64)) ([62b5327](https://github.com/cosmicspork/tracon/commit/62b53279d79b105247e1581d078fc686546c22ec))
* **spa:** make the interface installable ([#65](https://github.com/cosmicspork/tracon/issues/65)) ([4efe664](https://github.com/cosmicspork/tracon/commit/4efe6644f5b68abf155ef31006f15435398a198a))
* **wrapper:** a Tauri tray client for a running node ([#68](https://github.com/cosmicspork/tracon/issues/68)) ([3e50d79](https://github.com/cosmicspork/tracon/commit/3e50d794fba62a460aae357d8452a7aa6ed194c2))

## [0.4.0](https://github.com/cosmicspork/tracon/compare/v0.3.0...v0.4.0) (2026-08-28)


### Features

* **metrics:** per-channel daily ceiling, metrics rollups, provenance per commit, and channel bindings ([#59](https://github.com/cosmicspork/tracon/issues/59)) ([30a4b73](https://github.com/cosmicspork/tracon/commit/30a4b735c66917d1b7998e38bc0da94bd71fa23a))
* **review:** deterministic checks at submit, a diff cap, and a fresh review session whose verdict lands on the card ([#58](https://github.com/cosmicspork/tracon/issues/58)) ([954678d](https://github.com/cosmicspork/tracon/commit/954678dc7bdb4cf3dd553cb5188dd0d805c70e55))
* **session:** phases with a required ready item, plan artifact gate, and policy version on the row ([#57](https://github.com/cosmicspork/tracon/issues/57)) ([5a6f3d6](https://github.com/cosmicspork/tracon/commit/5a6f3d6d19eb6ef0805e629bb5649e65787225df))
* **spa:** Work screen and item view, the ready-work picker, checks and phases on sessions, review verdicts, channel meters, metrics ([#60](https://github.com/cosmicspork/tracon/issues/60)) ([0cec701](https://github.com/cosmicspork/tracon/commit/0cec701acc7ab567e0be9ec4728875bba8c41760))
* **sync:** work_item table, hash ids, and the deterministic ready-work order ([#55](https://github.com/cosmicspork/tracon/issues/55)) ([09a149c](https://github.com/cosmicspork/tracon/commit/09a149c76b60448c85ec67368900593557662f77))
* **work:** the ledger on the node: store, API, CLI, agent tools, and item close ends the session ([#56](https://github.com/cosmicspork/tracon/issues/56)) ([d7fcc15](https://github.com/cosmicspork/tracon/commit/d7fcc1597d64fd67104efd587225512aeb86ad67))


### Bug Fixes

* harden document and mesh trust boundaries ([#52](https://github.com/cosmicspork/tracon/issues/52)) ([19eb2ad](https://github.com/cosmicspork/tracon/commit/19eb2ad242b2ebe6e49c3c30a3a86e66eaaaa047))

## [0.3.0](https://github.com/cosmicspork/tracon/compare/v0.2.2...v0.3.0) (2026-08-28)


### Features

* **broker:** seal the credential store and hand credentials off over the mesh ([#37](https://github.com/cosmicspork/tracon/issues/37)) ([38c86c8](https://github.com/cosmicspork/tracon/commit/38c86c847b26cd075b8f05b20f646dec0b82b8dd))
* **corpus:** memory and document tools, bundle v3, corpus API and CLI ([#44](https://github.com/cosmicspork/tracon/issues/44)) ([92d3fc6](https://github.com/cosmicspork/tracon/commit/92d3fc6ee646abb8a902bd7c3775ea08894e42eb))
* **corpus:** per-session orientation from the corpus, the node, and the policy ([#45](https://github.com/cosmicspork/tracon/issues/45)) ([77ed9c5](https://github.com/cosmicspork/tracon/commit/77ed9c5e6d62ffa81e23d7c6475dccb6cb9fb223))
* **corpus:** replicated documents and memory, recall, project identity, mesh sync ([#43](https://github.com/cosmicspork/tracon/issues/43)) ([16ceccf](https://github.com/cosmicspork/tracon/commit/16ceccf6f4d97d43ada0d7387a74e570a81e6d4c))
* **gateway:** broker model credentials through a node-owned gateway ([#39](https://github.com/cosmicspork/tracon/issues/39)) ([6f47c4e](https://github.com/cosmicspork/tracon/commit/6f47c4ef23cb7a5cbe71624ca3677ce628d3a665))
* **hub:** encrypted snapshots to object storage, and restore ([#48](https://github.com/cosmicspork/tracon/issues/48)) ([39a910c](https://github.com/cosmicspork/tracon/commit/39a910cc155a0f58719dadddd9ccddd1ec587c69))
* **hub:** the hub as a replica for channels it is handed ([#46](https://github.com/cosmicspork/tracon/issues/46)) ([82037b1](https://github.com/cosmicspork/tracon/commit/82037b19ded793be13632ebb3ad19602b471fa9f))
* **memory:** nightly promotion batches through the approval queue ([#47](https://github.com/cosmicspork/tracon/issues/47)) ([a0fe86e](https://github.com/cosmicspork/tracon/commit/a0fe86e05f9472c2aa643de30de557a5428ac40c))
* **proto:** record changesets and contract version 2 ([#41](https://github.com/cosmicspork/tracon/issues/41)) ([036309f](https://github.com/cosmicspork/tracon/commit/036309f2dde7a0bc5cd70f2046d68adb4d3731f8))
* **providers:** connect a model provider through the harness's own login ([#40](https://github.com/cosmicspork/tracon/issues/40)) ([55e96ac](https://github.com/cosmicspork/tracon/commit/55e96acf0fa070f5480fef0850dedef986650f9b))
* **spa:** documents screen with search, markdown view, and a conflict-aware editor ([#49](https://github.com/cosmicspork/tracon/issues/49)) ([62e00b1](https://github.com/cosmicspork/tracon/commit/62e00b1e3255791bed15c56daa22bc68506cb2d9))
* **sync:** shared record schema, HLC, and last-writer-wins changesets ([#42](https://github.com/cosmicspork/tracon/issues/42)) ([ce74f49](https://github.com/cosmicspork/tracon/commit/ce74f49e3887ee8dfefa8df6bb9f330dec2ccc81))

## [0.2.2](https://github.com/cosmicspork/tracon/compare/v0.2.1...v0.2.2) (2026-08-28)


### Bug Fixes

* **mesh:** tell the hub which channels this node holds before granting them ([f2cd842](https://github.com/cosmicspork/tracon/commit/f2cd842bc8f7b779b84acdd709bfbed8d874377a))
* **mesh:** tell the hub which channels this node holds before granting them ([acd22ad](https://github.com/cosmicspork/tracon/commit/acd22ad3fef594cd842b5e0f3dbd32d22a8e024e))

## [0.2.1](https://github.com/cosmicspork/tracon/compare/v0.2.0...v0.2.1) (2026-08-28)


### Bug Fixes

* **deploy:** grant get on pods/attach — the websocket attach is a GET ([a0f0de3](https://github.com/cosmicspork/tracon/commit/a0f0de35ce713472fc57d631983da9fc6ca2ddd6))
* **node:** pin the gated probe pod by hostname label, not nodeName ([54ca9c0](https://github.com/cosmicspork/tracon/commit/54ca9c05f17c330491b3238884ff683cd8f0d6f4))
* **node:** pin the gated probe pod by hostname label, not nodeName ([ae5c08d](https://github.com/cosmicspork/tracon/commit/ae5c08ded32d2d9b807c0882ad18f9cbde1479a9))
* **node:** pin the gated probe pod by hostname label, not nodeName ([8fb31db](https://github.com/cosmicspork/tracon/commit/8fb31db2dd7521c035efbb0a259aded886910f17))

## [0.2.0](https://github.com/cosmicspork/tracon/compare/v0.1.0...v0.2.0) (2026-08-28)


### Features

* **broker:** gitlab and jira as narrow brokered tools ([2f8cce7](https://github.com/cosmicspork/tracon/commit/2f8cce7894388b11d8ff665416198f8e8b1070ae))
* **broker:** gitlab and jira as narrow brokered tools ([a1e0591](https://github.com/cosmicspork/tracon/commit/a1e0591e7d141bc10b88c90d271b1b53e9d39a60))
* **broker:** node bindings and policy on every brokered tool call ([20760bc](https://github.com/cosmicspork/tracon/commit/20760bc49c5572b1d5ae2fdcec2011cbebad64cd))
* **broker:** node bindings and policy on every brokered tool call ([ae88722](https://github.com/cosmicspork/tracon/commit/ae88722403d4b91313e2a527cd2ac6baf6469c48))
* credential broker and the node's first brokered tool ([d791a1d](https://github.com/cosmicspork/tracon/commit/d791a1d9151989c53a0fc111aeb25c3fa6d8f129))
* credential broker and the node's first brokered tool ([24a5aa6](https://github.com/cosmicspork/tracon/commit/24a5aa67b5a78e6c800364cb7e1ac919834ed42a))
* **enroll:** invitations, key and policy handoff, hot-reloaded policy ([97c879f](https://github.com/cosmicspork/tracon/commit/97c879fc5ac0677c540f02b4cfa9b146e24c8513))
* **enroll:** invitations, key and policy handoff, hot-reloaded policy ([9750170](https://github.com/cosmicspork/tracon/commit/97501702d98e4a18502a70fed61d4da777711e47))
* **hub:** relay crate, image, and release pipeline ([5864b2d](https://github.com/cosmicspork/tracon/commit/5864b2d47cb46e13015b0d02345299980a1366ef))
* **hub:** relay crate, image, and release pipeline ([293a189](https://github.com/cosmicspork/tracon/commit/293a18946e801b12702b5c2ed9611fd9ed37875a))
* **mesh:** forward commands to session owners and mesh-aware interface ([fb36ec2](https://github.com/cosmicspork/tracon/commit/fb36ec27d2dc2a0b8aba3f891a15e892c95e40e9))
* **mesh:** forward commands to session owners and mesh-aware interface ([ab04cbd](https://github.com/cosmicspork/tracon/commit/ab04cbd2abe0ea25270d439a610d44b41acc0bef))
* **mesh:** hub client with outbox, cursor pull, mirroring, and presence ([1ad7f0b](https://github.com/cosmicspork/tracon/commit/1ad7f0b3205f13cd690882d2343a331094447575))
* **mesh:** hub client with outbox, cursor pull, mirroring, and presence ([8fb1ec4](https://github.com/cosmicspork/tracon/commit/8fb1ec4bbb8b48170a32ad09716818abd3f4f0f8))
* **node:** kubernetes runtime backend ([0e6dc7f](https://github.com/cosmicspork/tracon/commit/0e6dc7f9032b54932ed0f9c7eb005a93da9cbd28))
* **node:** kubernetes runtime backend — harness pods, attach, connect proxy, checks ([ffe2afc](https://github.com/cosmicspork/tracon/commit/ffe2afcfa022658e6763ecb120241a3ec5a3eb2d))
* **node:** node-owned harness volume, Linux socket forward, embedded images, install.sh ([c4428c8](https://github.com/cosmicspork/tracon/commit/c4428c809f78c2a377be4e3a19eb07f06843122d))
* **node:** node-owned harness volume, Linux socket forward, embedded images, install.sh ([ba53719](https://github.com/cosmicspork/tracon/commit/ba53719d2787a319a39eaee224bd630728202cac))
* podman runner and boundary checks ([29937e7](https://github.com/cosmicspork/tracon/commit/29937e76d9aae2c9bc2622296e027586f48e8c8f))
* policy, and the five working agreements as rules ([90d2b91](https://github.com/cosmicspork/tracon/commit/90d2b919f3d65ce3c4fdb36dcb83caf816ad234e))
* policy, and the five working agreements as rules ([29fb80d](https://github.com/cosmicspork/tracon/commit/29fb80d66814296bbc564a293a964231626c875a))
* **proto:** mesh wire contract crate ([85b5bf5](https://github.com/cosmicspork/tracon/commit/85b5bf51b9be699e7654f8596384a1f486c03e78))
* **proto:** mesh wire contract crate with pinned vectors ([9c5699e](https://github.com/cosmicspork/tracon/commit/9c5699e03afe8e5242e7bac26c244d46401c59f2))
* review before publish, enforced ([cc5eb81](https://github.com/cosmicspork/tracon/commit/cc5eb811543460c4f06a1ff613a466c81e04398e))
* review before publish, enforced ([de46459](https://github.com/cosmicspork/tracon/commit/de464597954e6187d0989bef7c16779347e44d41))
* sessions, permissions, budget, and the event stream ([dd4501b](https://github.com/cosmicspork/tracon/commit/dd4501b7b2f62c1ab3c283b29166443660a474f4))
* store, acp codec, and omp adapter ([0b6d870](https://github.com/cosmicspork/tracon/commit/0b6d8703ad14bbe21b61855a7ecdedc5e651adae))
* the interface, and honest restarts ([20a64ad](https://github.com/cosmicspork/tracon/commit/20a64ad8c0e9493ad525a11dcd8b34ea1675b310))
* the interface, and honest restarts ([58cbb15](https://github.com/cosmicspork/tracon/commit/58cbb15717f6b2ee749e25ab6eeacb1408a77473))
* the phone side, and a design record that matches what was built ([adb261d](https://github.com/cosmicspork/tracon/commit/adb261d03c233f7a495cd346df162dde23eebfa9))
* the phone side, and a design record that matches what was built ([03bafaa](https://github.com/cosmicspork/tracon/commit/03bafaaea2d8cab234aecdd8730dea8d0ccfa4c3))
* tool surface reduction, request-changes, and static musl builds ([db6edd5](https://github.com/cosmicspork/tracon/commit/db6edd50084a1a4779e6b266b3337496a083d065))
* tool surface reduction, request-changes, and static musl builds ([36bbc24](https://github.com/cosmicspork/tracon/commit/36bbc245d5129e0eb6158bede1889bc4fc2cb3d8))


### Bug Fixes

* **boundary:** stop the harness reaching what the node holds ([dea0cd9](https://github.com/cosmicspork/tracon/commit/dea0cd91f742093e453960c3b84bc7e4fc8ff003))
* **gate:** tighten policy, the SQL guard, and credential handling ([96c4f4e](https://github.com/cosmicspork/tracon/commit/96c4f4ed04ed31d7cb2e66ed343ff1d9ef54eae4))
* **review:** make approve atomic and honest ([4add16e](https://github.com/cosmicspork/tracon/commit/4add16e593dd197653570e4272c0acaef731da7d))
* select tracon binary in just recipes ([301afbf](https://github.com/cosmicspork/tracon/commit/301afbff54d9c0d6f9eca3b3d3d90a2cf00f0f16))
* **session:** fail closed, clean up, and do not deadlock or stall ([d4cf7ae](https://github.com/cosmicspork/tracon/commit/d4cf7aef5090716335b02075aab7ac1bd2e00db7))
* **spa:** classify diffs by hunk, deliver node frames, fix races ([f780b34](https://github.com/cosmicspork/tracon/commit/f780b34db0f10cb429cb519d07bb69df92380a1a))
