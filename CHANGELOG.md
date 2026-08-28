# Changelog

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
