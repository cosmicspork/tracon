# Changelog

## [0.30.0](https://github.com/cosmicspork/tracon/compare/v0.29.0...v0.30.0) (2026-10-09)


### Features

* **session:** name the session's forge repository and default the forge tools and submit_review to it ([#416](https://github.com/cosmicspork/tracon/issues/416)) ([134a98f](https://github.com/cosmicspork/tracon/commit/134a98fe33a7b4d4661c24609cf5bcd838f353cc))
* **spa:** render a session's messages as Markdown ([#415](https://github.com/cosmicspork/tracon/issues/415)) ([454c2cd](https://github.com/cosmicspork/tracon/commit/454c2cdf1b507e654935f63e3bf7601c915f108a))


### Bug Fixes

* **approvals:** title gated tools in words and give service_start a schema ([#427](https://github.com/cosmicspork/tracon/issues/427)) ([a8aab85](https://github.com/cosmicspork/tracon/commit/a8aab85820f87d87985919656527aef2ea22feb0))
* **boundary:** clone volumes without copying SELinux labels ([#412](https://github.com/cosmicspork/tracon/issues/412)) ([de5d815](https://github.com/cosmicspork/tracon/commit/de5d8155ca1b1b50e571d1a731dccfc835ee2a7d))
* **harness:** pin Claude Code 2.1.295 ([#434](https://github.com/cosmicspork/tracon/issues/434)) ([43952c2](https://github.com/cosmicspork/tracon/commit/43952c2879919421238f9109487b0935cb567895))
* **mcp:** return ask_operator within the wait budget and let question_status wait ([#425](https://github.com/cosmicspork/tracon/issues/425)) ([6db39e8](https://github.com/cosmicspork/tracon/commit/6db39e8f2944ed21083840d92bcbef1680cbb568))
* **mcp:** skip question_status in the asked-tool title guard ([#430](https://github.com/cosmicspork/tracon/issues/430)) ([52ba2a9](https://github.com/cosmicspork/tracon/commit/52ba2a9acf1afc84ee79f540eab74be220565224))
* **node:** read a process's start time from /proc on Linux ([#413](https://github.com/cosmicspork/tracon/issues/413)) ([33fcef2](https://github.com/cosmicspork/tracon/commit/33fcef2926a7a63b3c678baebddb15d8e6c4babb))
* **providers:** hide the unkeyed built-in API provider and take a declared one's key in place ([#432](https://github.com/cosmicspork/tracon/issues/432)) ([a32256a](https://github.com/cosmicspork/tracon/commit/a32256a8b0d4d6739d0135db6f9681488927ffbf))
* **review:** keep the base a squashed update merged as a second parent ([#429](https://github.com/cosmicspork/tracon/issues/429)) ([7983cd1](https://github.com/cosmicspork/tracon/commit/7983cd1737b1dfb4f0c23db35c0e2a05de359790))
* **review:** let a published review update the change it opened ([#428](https://github.com/cosmicspork/tracon/issues/428)) ([e09cd92](https://github.com/cosmicspork/tracon/commit/e09cd9253b5ee982d40a9fe60b0fc9a1c93b2088))
* **review:** run submit_review's checks on the node and let review_status wait ([#417](https://github.com/cosmicspork/tracon/issues/417)) ([12af162](https://github.com/cosmicspork/tracon/commit/12af1629eff25276203e49980f5b7a92a79d3e23))
* **review:** say what a failed check failed on, ahead of its tail ([#433](https://github.com/cosmicspork/tracon/issues/433)) ([ae2809d](https://github.com/cosmicspork/tracon/commit/ae2809da4e76765b0ead66490afb5da07ca361ad))
* **spa:** confirm memory deletes and trim home prose ([#408](https://github.com/cosmicspork/tracon/issues/408)) ([bf52a73](https://github.com/cosmicspork/tracon/commit/bf52a73a0b6ee69ca0388f25b59be36d97d05ff4))
* **spa:** give every session event kind a readable log line ([#420](https://github.com/cosmicspork/tracon/issues/420)) ([4fa235d](https://github.com/cosmicspork/tracon/commit/4fa235d39befa6532ae99da1c7263ab986250adc))
* **spa:** keep policy records from splitting a run of tool calls ([#411](https://github.com/cosmicspork/tracon/issues/411)) ([56f4169](https://github.com/cosmicspork/tracon/commit/56f416943f5bc6e050c6feb4fba8fe3a747171e7))
* **spa:** keep the permission card's title and answers on a phone ([#421](https://github.com/cosmicspork/tracon/issues/421)) ([072eaa0](https://github.com/cosmicspork/tracon/commit/072eaa0d9572f24932172dfe1c423cebc5db9d65))
* **spa:** keep the review verdict bar in view on a phone and show a decided review as decided ([#424](https://github.com/cosmicspork/tracon/issues/424)) ([953762f](https://github.com/cosmicspork/tracon/commit/953762fbb82afcea28694b2d217a0b99fb51521b))
* **spa:** list context documents and any kind the list does not name ([#419](https://github.com/cosmicspork/tracon/issues/419)) ([342282b](https://github.com/cosmicspork/tracon/commit/342282bec705dd4b7e9fb305812041f04621b566))
* **spa:** show declared models, keep repository commits and style, untick a failed push enrolment ([#423](https://github.com/cosmicspork/tracon/issues/423)) ([70efbe6](https://github.com/cosmicspork/tracon/commit/70efbe69b5b30ed7675cd9bceebf1ed368cb8bf0))
* **spa:** tell loading and failed lists apart from empty ones ([#426](https://github.com/cosmicspork/tracon/issues/426)) ([5733c95](https://github.com/cosmicspork/tracon/commit/5733c95b1d942600e2eca5c7223a8db51c88e58e))
* **work:** execute a written plan on Continue and offer only planning before one ([#422](https://github.com/cosmicspork/tracon/issues/422)) ([67a0989](https://github.com/cosmicspork/tracon/commit/67a0989f92d4f858a53b481c86d9e10b9221fa6b))

## [0.29.0](https://github.com/cosmicspork/tracon/compare/v0.28.0...v0.29.0) (2026-10-07)


### Features

* **boundary:** run the Podman gateway as its own user service, and start it when found stopped ([#377](https://github.com/cosmicspork/tracon/issues/377)) ([1d39a26](https://github.com/cosmicspork/tracon/commit/1d39a2682ef08c2d0926751213efee0bcad5c66c))
* draft, try and propose a repository's entry from a session ([#391](https://github.com/cosmicspork/tracon/issues/391)) ([b32a9e6](https://github.com/cosmicspork/tracon/commit/b32a9e6cba51bbf853f85ad4dac8d2f9bbfb79e9))
* follow a published request on its forge until it closes ([#393](https://github.com/cosmicspork/tracon/issues/393)) ([ab7d644](https://github.com/cosmicspork/tracon/commit/ab7d6444fccb7b27b763c46a52a40f13b9a25c56))
* give GitHub the CI tools GitLab has ([#399](https://github.com/cosmicspork/tracon/issues/399)) ([18b207a](https://github.com/cosmicspork/tracon/commit/18b207a11ae3eaae339d71125bd1112a62ef32c6))
* **jira:** list an issue's available transitions ([#401](https://github.com/cosmicspork/tracon/issues/401)) ([987dbee](https://github.com/cosmicspork/tracon/commit/987dbee4ace8ee3013e15e2bbc81f9309a4fff34))
* keep a review's unsent feedback and prose on the node ([#395](https://github.com/cosmicspork/tracon/issues/395)) ([7450eab](https://github.com/cosmicspork/tracon/commit/7450eab311ff47177ffe538cda503522d8c80949))
* keep a session's build output in a per-repository cache ([#390](https://github.com/cosmicspork/tracon/issues/390)) ([88fd338](https://github.com/cosmicspork/tracon/commit/88fd338bf887ea3ed8373ae7fd97325a1ea39adb))
* keep the verdict bar in reach and say why an action is unavailable ([#397](https://github.com/cosmicspork/tracon/issues/397)) ([8d265f8](https://github.com/cosmicspork/tracon/commit/8d265f8e848c6f916b6ce2f1ccf1f5ef00e16a78))
* **node:** keep the machine awake while a session works, and count only awake time against a card ([#379](https://github.com/cosmicspork/tracon/issues/379)) ([49fd41f](https://github.com/cosmicspork/tracon/commit/49fd41f7c479a5d7537de1e7b408d61da612dfd7))
* outcome record derived from recorded state ([#386](https://github.com/cosmicspork/tracon/issues/386)) ([003ca98](https://github.com/cosmicspork/tracon/commit/003ca986e8ff236bdfcf9706934bb96c6cb386ed))
* preview preparation and explain what a repository asks that the node will not do ([#388](https://github.com/cosmicspork/tracon/issues/388)) ([fba6477](https://github.com/cosmicspork/tracon/commit/fba647767f98f27c8c74bb0778e5a66cdc3f665c))
* publish as the operator's forge account, not as tracon ([#382](https://github.com/cosmicspork/tracon/issues/382)) ([c9cc82f](https://github.com/cosmicspork/tracon/commit/c9cc82f4946c4a87086158efdd047d8d717593ff))
* read a mirrored review's detail from the node that holds it ([#392](https://github.com/cosmicspork/tracon/issues/392)) ([0383ee5](https://github.com/cosmicspork/tracon/commit/0383ee527fb187804bdf6cb6d6d57d3c51736a5b))
* **review:** show publication readiness and recover a failed publish ([#381](https://github.com/cosmicspork/tracon/issues/381)) ([f61e53b](https://github.com/cosmicspork/tracon/commit/f61e53bf2be4c56a4eb0b73c663e5180ab1e9915))
* say what investigating, verifying and publishing would lack ([#387](https://github.com/cosmicspork/tracon/issues/387)) ([d45d666](https://github.com/cosmicspork/tracon/commit/d45d666e7ecc27532d36ca1923e7ca6c74e4ae07))
* service sidecars, started by name from a node catalogue ([#389](https://github.com/cosmicspork/tracon/issues/389)) ([7ce2ceb](https://github.com/cosmicspork/tracon/commit/7ce2ceb52c3e0844d4c3333b6c4f8fa6497c0ea2))
* **session:** a node restart ends a session as node_restart, and offers to continue it ([#375](https://github.com/cosmicspork/tracon/issues/375)) ([f46e8e7](https://github.com/cosmicspork/tracon/commit/f46e8e7122fd4890045ab2d74dee39ce61ed2307))
* **session:** choose what happens when a provider is exhausted ([#384](https://github.com/cosmicspork/tracon/issues/384)) ([bb8d36d](https://github.com/cosmicspork/tracon/commit/bb8d36dfb9229186d2636e667fdfda0a77207d7b))
* **session:** record a policy decision for every Claude Code tool call ([#383](https://github.com/cosmicspork/tracon/issues/383)) ([7fe0ea3](https://github.com/cosmicspork/tracon/commit/7fe0ea377d3c31e173592ef0f45a30376fd96557))
* **session:** record a publication on its session, and suspend a published session that goes idle ([#380](https://github.com/cosmicspork/tracon/issues/380)) ([4481a23](https://github.com/cosmicspork/tracon/commit/4481a23e57d9ae6b013ef4b3d7c2fb080568da26))
* **settings:** show what the node holds, kind by kind ([#402](https://github.com/cosmicspork/tracon/issues/402)) ([03a9b92](https://github.com/cosmicspork/tracon/commit/03a9b9270eb589c392bcdaf1df2b87ee85e5380d))
* ship one commit of the reviewed tree with the approved message and branch ([#394](https://github.com/cosmicspork/tracon/issues/394)) ([7b55a4d](https://github.com/cosmicspork/tracon/commit/7b55a4d29cf344d6f1e5175bc0a4289b721ed2ad))
* show what changed since the last verdict beside the full diff ([#396](https://github.com/cosmicspork/tracon/issues/396)) ([ad7a9c7](https://github.com/cosmicspork/tracon/commit/ad7a9c7f1bdc92f76e646f025aab02d643cb20d4))
* **work:** a continuation view for any piece of work, with continue, change approach and abandon ([#385](https://github.com/cosmicspork/tracon/issues/385)) ([8c2709c](https://github.com/cosmicspork/tracon/commit/8c2709ceebabc7a98851f8bb1060ee2976424269))


### Bug Fixes

* **desktop:** open external links with the host's launcher and the host's environment ([#378](https://github.com/cosmicspork/tracon/issues/378)) ([98605f9](https://github.com/cosmicspork/tracon/commit/98605f9d6f31d09a3f49e9f059409e67a2c1a436))
* **spa:** say where push enrollment failed ([#403](https://github.com/cosmicspork/tracon/issues/403)) ([6d4c208](https://github.com/cosmicspork/tracon/commit/6d4c20850ec2e200948501395a0bb7d91a6a1c67))

## [0.28.0](https://github.com/cosmicspork/tracon/compare/v0.27.0...v0.28.0) (2026-10-06)


### ⚠ BREAKING CHANGES

* **external:** answer external harnesses with no session ([#358](https://github.com/cosmicspork/tracon/issues/358))
* **store:** let reviews, approvals and questions name no session ([#357](https://github.com/cosmicspork/tracon/issues/357))

### Features

* **approvals:** reasons, changes_requested, schema-checked edits, approval API ([#361](https://github.com/cosmicspork/tracon/issues/361)) ([#367](https://github.com/cosmicspork/tracon/issues/367)) ([b6f46e7](https://github.com/cosmicspork/tracon/commit/b6f46e752f955b725dcb11ad37b14107f2dc3438))
* **egress:** ask the operator before a session's egress, rather than refusing it ([#372](https://github.com/cosmicspork/tracon/issues/372)) ([5eea44d](https://github.com/cosmicspork/tracon/commit/5eea44d1221219f8032224d8fe3a6814fe7c6a32))
* **external:** answer external harnesses with no session ([#358](https://github.com/cosmicspork/tracon/issues/358)) ([f3b0707](https://github.com/cosmicspork/tracon/commit/f3b07076cf569b477a519412cbf6d7d99790d2c3))
* **external:** show your own agents by lane on the home ([#359](https://github.com/cosmicspork/tracon/issues/359)) ([dae698d](https://github.com/cosmicspork/tracon/commit/dae698d63df4c3abd86aa0429642a212d8efd618))
* operator Jira search route for local clients ([#353](https://github.com/cosmicspork/tracon/issues/353)) ([070cc31](https://github.com/cosmicspork/tracon/commit/070cc31a7fbebd0a2cc4dc9b98d5502947087831))
* **review:** show_work, the agent's account of its work beside the diff ([#371](https://github.com/cosmicspork/tracon/issues/371)) ([9d3ad3a](https://github.com/cosmicspork/tracon/commit/9d3ad3a918f92a8a435a30d9fab05a55c7a6dac1))
* **session:** a work item's session starts working on its own ([#373](https://github.com/cosmicspork/tracon/issues/373)) ([05ffda7](https://github.com/cosmicspork/tracon/commit/05ffda74962f5823a09abe9bfdf5bfbe6e65bcaf))
* **spa:** full-page approval view for prose writes ([#361](https://github.com/cosmicspork/tracon/issues/361)) ([#370](https://github.com/cosmicspork/tracon/issues/370)) ([c4edb7e](https://github.com/cosmicspork/tracon/commit/c4edb7e8cc6baa051825e91b991205919eae4b48))
* **spa:** Jira wiki renderer, schema form model, and text diff ([#361](https://github.com/cosmicspork/tracon/issues/361)) ([#363](https://github.com/cosmicspork/tracon/issues/363)) ([affe722](https://github.com/cosmicspork/tracon/commit/affe722932db53e71c8d1537420d02f3e6b80782))
* **store:** let reviews, approvals and questions name no session ([#357](https://github.com/cosmicspork/tracon/issues/357)) ([8827c34](https://github.com/cosmicspork/tracon/commit/8827c34628f36957c679de169ebdf7a8a1cd84a7))
* track report_issue drafts with issue_report_status ([#362](https://github.com/cosmicspork/tracon/issues/362)) ([#366](https://github.com/cosmicspork/tracon/issues/366)) ([4c9679f](https://github.com/cosmicspork/tracon/commit/4c9679f71351cd74bc856a6304b92aa232dfb04c))


### Bug Fixes

* **policy:** drop no-production-deploy and keep prose out of deny matching ([#360](https://github.com/cosmicspork/tracon/issues/360)) ([#364](https://github.com/cosmicspork/tracon/issues/364)) ([2bdb2fd](https://github.com/cosmicspork/tracon/commit/2bdb2fd762343696d5bb34f68f0c9d7e25fbecf4))
* **review:** a same-commit retry that names another base retargets ([#356](https://github.com/cosmicspork/tracon/issues/356)) ([#369](https://github.com/cosmicspork/tracon/issues/369)) ([2ac5edd](https://github.com/cosmicspork/tracon/commit/2ac5eddb183b2f670e5dd553f0600cf1717ef351))
* **review:** let a resubmission retarget its base and rewrite an unopened branch ([#356](https://github.com/cosmicspork/tracon/issues/356)) ([#365](https://github.com/cosmicspork/tracon/issues/365)) ([185a5ab](https://github.com/cosmicspork/tracon/commit/185a5abee400198586eea717c30fae8f1b182712))
* **spa:** stop the review card styles leaking onto the decision bar ([#352](https://github.com/cosmicspork/tracon/issues/352)) ([cc995cf](https://github.com/cosmicspork/tracon/commit/cc995cf8e5f5dc914f63c7b60b3f38470af0b311))


### Performance Improvements

* **review:** bound submit_review latency for external harnesses ([#355](https://github.com/cosmicspork/tracon/issues/355)) ([#368](https://github.com/cosmicspork/tracon/issues/368)) ([5bfec2e](https://github.com/cosmicspork/tracon/commit/5bfec2e90f054dd7ae7b618ba9f0384de8506332))

## [0.27.0](https://github.com/cosmicspork/tracon/compare/v0.26.1...v0.27.0) (2026-10-05)


### Features

* **broker:** hold operator-decided calls as approvals instead of blocking ([#349](https://github.com/cosmicspork/tracon/issues/349)) ([d18bea0](https://github.com/cosmicspork/tracon/commit/d18bea0107a1e7aab7fb419bce704791f0c28bb6))
* **external:** label an external agent's calls with its lane ([#348](https://github.com/cosmicspork/tracon/issues/348)) ([427109a](https://github.com/cosmicspork/tracon/commit/427109a796e18fa8decf089b94cbe5adb705e62f))


### Bug Fixes

* **external:** answer the server/discover probe without attaching a session ([#347](https://github.com/cosmicspork/tracon/issues/347)) ([a48a38e](https://github.com/cosmicspork/tracon/commit/a48a38eaf10fa27587c17ef45277f27a7e3dfa75))
* **mcp:** send brokered writes exactly as approved ([#345](https://github.com/cosmicspork/tracon/issues/345)) ([14dbbc1](https://github.com/cosmicspork/tracon/commit/14dbbc1090c49cb06102cf67f0f329b94161d14a))
* **publish:** publish exactly the approved description and comment ([#346](https://github.com/cosmicspork/tracon/issues/346)) ([f097af9](https://github.com/cosmicspork/tracon/commit/f097af96939a0ea41e077b529449c8fddbac67f4))
* **review:** report changed files, not changed lines, from submit_review ([#344](https://github.com/cosmicspork/tracon/issues/344)) ([218e32c](https://github.com/cosmicspork/tracon/commit/218e32cd6b876aa3cedc82667dbe2f3577bf51eb))
* **service:** reload the LaunchAgent on restart so an updated binary starts ([#342](https://github.com/cosmicspork/tracon/issues/342)) ([bd1b504](https://github.com/cosmicspork/tracon/commit/bd1b504f2bb9fac5bddc32f43c097fd5f4fc43e0))

## [0.26.1](https://github.com/cosmicspork/tracon/compare/v0.26.0...v0.26.1) (2026-10-05)


### Bug Fixes

* **review:** read a mirrored review without judging it by this node's disk ([#340](https://github.com/cosmicspork/tracon/issues/340)) ([1dae6d9](https://github.com/cosmicspork/tracon/commit/1dae6d94dd5431cbef2eb57198120389ca7bdf91))

## [0.26.0](https://github.com/cosmicspork/tracon/compare/v0.25.0...v0.26.0) (2026-10-04)


### Features

* **gateway:** give each egress client its own grant ([#319](https://github.com/cosmicspork/tracon/issues/319)) ([8a6fe21](https://github.com/cosmicspork/tracon/commit/8a6fe21aab79d33b4d53de688b75567b72a2e35a))
* **policy:** ask less, and refuse Claude Code settings carried by the workspace ([#313](https://github.com/cosmicspork/tracon/issues/313)) ([ef688a4](https://github.com/cosmicspork/tracon/commit/ef688a411dede37235491887e737b4479a74e55d))
* **policy:** let the boundary contain execution, ask about what leaves ([#337](https://github.com/cosmicspork/tracon/issues/337)) ([de08a34](https://github.com/cosmicspork/tracon/commit/de08a3416244355547c877c14f41d1fbe796d454))
* **repo:** build a repository's toolchain image from its default-branch Dockerfile ([#317](https://github.com/cosmicspork/tracon/issues/317)) ([2c1c1d1](https://github.com/cosmicspork/tracon/commit/2c1c1d14ecc804e018eb8ef5885d42cdcf761468))
* **review:** prepare once per candidate, in a cache no other candidate wrote ([#321](https://github.com/cosmicspork/tracon/issues/321)) ([5dfc9b3](https://github.com/cosmicspork/tracon/commit/5dfc9b3c5f59fc12a5840418b353516da443f4a0))
* **review:** run required checks in the repository's own environment ([#314](https://github.com/cosmicspork/tracon/issues/314)) ([5a2bcd4](https://github.com/cosmicspork/tracon/commit/5a2bcd4d15b3379e4b5571311e4106360be39d7a))
* **session:** open a repository's registries to its sessions ([#320](https://github.com/cosmicspork/tracon/issues/320)) ([120a399](https://github.com/cosmicspork/tracon/commit/120a3993361254cff665eace6b39c714b6e2856c))
* **session:** run a Claude session in its repository's image ([#318](https://github.com/cosmicspork/tracon/issues/318)) ([ead490c](https://github.com/cosmicspork/tracon/commit/ead490ce0aeb2431623ca3f3e15f90b620a9ac21))
* **settings:** the repository table in Settings and the CLI ([#323](https://github.com/cosmicspork/tracon/issues/323)) ([815a7f3](https://github.com/cosmicspork/tracon/commit/815a7f3663b3f62fd438de97fcbb74599e686b6e))


### Bug Fixes

* **adapter:** record a Claude Code tool's output in the session ledger ([#336](https://github.com/cosmicspork/tracon/issues/336)) ([275e23f](https://github.com/cosmicspork/tracon/commit/275e23f8e47a5dda648656433a1ae892bf479d7c))
* **config:** stop concurrent node.toml edits losing each other ([#329](https://github.com/cosmicspork/tracon/issues/329)) ([62ca620](https://github.com/cosmicspork/tracon/commit/62ca620b50b66a11440649801a4f7b06a2464ae0))
* **runner:** keep a run's own loopback off the proxy ([#322](https://github.com/cosmicspork/tracon/issues/322)) ([b4ce123](https://github.com/cosmicspork/tracon/commit/b4ce1235eadaf7ce7fa40ca88da61ea7e8df3101))
* **session:** let a harness ride out the retries it bounds itself before the watchdog pauses ([#311](https://github.com/cosmicspork/tracon/issues/311)) ([55c173b](https://github.com/cosmicspork/tracon/commit/55c173bb4e46e6cb4acf57f99d1755c409c7c358))
* **spa:** leave a verdict's navigation to whoever is still on the review ([#333](https://github.com/cosmicspork/tracon/issues/333)) ([856bfe1](https://github.com/cosmicspork/tracon/commit/856bfe184729ece07d445ca524002ca7bd06d398))
* **test:** let review fixtures plant replace refs inside a session ([#335](https://github.com/cosmicspork/tracon/issues/335)) ([b980c11](https://github.com/cosmicspork/tracon/commit/b980c1149e21cac07a70ea6e240dd295d7dea3d0))
* **usage:** charge new work, not cache reads, and settle a turn the session is stopped under ([#315](https://github.com/cosmicspork/tracon/issues/315)) ([0c9b76f](https://github.com/cosmicspork/tracon/commit/0c9b76fbc6f022b63c9da83f2358486226d084f2))
* **workspace:** export a workspace as Git sees it ([#316](https://github.com/cosmicspork/tracon/issues/316)) ([962d109](https://github.com/cosmicspork/tracon/commit/962d109c81b6d988164f69e6752aefa5db061e51))

## [0.25.0](https://github.com/cosmicspork/tracon/compare/v0.24.0...v0.25.0) (2026-09-30)


### Features

* **harness:** derive each session's harness from the credential its model runs on ([#309](https://github.com/cosmicspork/tracon/issues/309)) ([8e2a804](https://github.com/cosmicspork/tracon/commit/8e2a8044e015c58debbdafeeee9a69ade5b8f0ef))
* **harness:** run each session on the harness it names, with an image for every supported one ([#307](https://github.com/cosmicspork/tracon/issues/307)) ([70992d8](https://github.com/cosmicspork/tracon/commit/70992d8b5c8ce91a0667a4fe80be2f2d97558ead))
* **harness:** show which harness each model runs on, and let an either-way model choose ([#310](https://github.com/cosmicspork/tracon/issues/310)) ([d022cd0](https://github.com/cosmicspork/tracon/commit/d022cd04e0393e7b644cd666fd9d933310f26fe6))
* **opencode:** drive sessions over the v1 routes, where the node's MCP tools reach the model ([#306](https://github.com/cosmicspork/tracon/issues/306)) ([5165202](https://github.com/cosmicspork/tracon/commit/5165202f03abfb9422c6015661653b8a8c92904a))


### Bug Fixes

* **criteria:** record an omitted provenance as the inferred the tool promises ([#299](https://github.com/cosmicspork/tracon/issues/299)) ([2081699](https://github.com/cosmicspork/tracon/commit/2081699596f4c09f2e234d87ab0dee38d6d5442c))

## [0.24.0](https://github.com/cosmicspork/tracon/compare/v0.23.1...v0.24.0) (2026-09-29)


### Features

* **criteria:** bind acceptance criteria to checks, scenarios, observations and verdicts ([#290](https://github.com/cosmicspork/tracon/issues/290)) ([db81c6a](https://github.com/cosmicspork/tracon/commit/db81c6a9b5d07cddf9874be4476e1ad4bd0ad571))
* **mcp:** read and answer forge review threads ([#298](https://github.com/cosmicspork/tracon/issues/298)) ([9dc2a61](https://github.com/cosmicspork/tracon/commit/9dc2a61750db7a3d62d5d25ec96bcbec7c1dc396))
* **review:** update an existing pull or merge request from the review gate ([#297](https://github.com/cosmicspork/tracon/issues/297)) ([812acbb](https://github.com/cosmicspork/tracon/commit/812acbb088958890927f9e47271d89dec98819d3))


### Bug Fixes

* **kubernetes:** let the node's pod reach an OpenCode harness, and nothing else ([#294](https://github.com/cosmicspork/tracon/issues/294)) ([99767f7](https://github.com/cosmicspork/tracon/commit/99767f7141d4362970b43653260fde6df5a756d7))
* **mcp:** route job_play and pipeline_list_by_sha to their handler ([#296](https://github.com/cosmicspork/tracon/issues/296)) ([a352f2d](https://github.com/cosmicspork/tracon/commit/a352f2d4f8a716c0fd0861368217d9a081ac0339))
* **spa:** make the desktop Check for updates button actually check ([#293](https://github.com/cosmicspork/tracon/issues/293)) ([ec5f68a](https://github.com/cosmicspork/tracon/commit/ec5f68a33e389514a9b1f5d9afbad3896ff4463c))

## [0.23.1](https://github.com/cosmicspork/tracon/compare/v0.23.0...v0.23.1) (2026-09-29)


### Bug Fixes

* **claude:** let a call to the node's MCP server outlast an operator's answer ([#289](https://github.com/cosmicspork/tracon/issues/289)) ([68b8e60](https://github.com/cosmicspork/tracon/commit/68b8e60e5f99a49caafa6d99de913eaf1bfdde0a))
* **claude:** route Claude Code's permission asks to the node over stdio ([#286](https://github.com/cosmicspork/tracon/issues/286)) ([d7e7e62](https://github.com/cosmicspork/tracon/commit/d7e7e62ffdea9e1c94ea5e84b9ed8d3b557b0315))
* **claude:** send Claude Code the model as Anthropic names it, and re-probe models once the boundary passes ([#284](https://github.com/cosmicspork/tracon/issues/284)) ([76f0722](https://github.com/cosmicspork/tracon/commit/76f0722cd7f8adaaa54b9ca70f8dd275d6898627))
* **compose:** refuse execute before writing an item that cannot have a plan ([#280](https://github.com/cosmicspork/tracon/issues/280)) ([f6b1420](https://github.com/cosmicspork/tracon/commit/f6b1420ae228d96d3b2b83232cf69e1b42e8b311))
* **config:** offer the ChatGPT subscription only models it serves ([#279](https://github.com/cosmicspork/tracon/issues/279)) ([3150213](https://github.com/cosmicspork/tracon/commit/3150213cf859808b6ca7212588e4dc60759d3c76))
* **config:** save only what changed, so a default is never frozen into node.toml ([#292](https://github.com/cosmicspork/tracon/issues/292)) ([cfa4b1a](https://github.com/cosmicspork/tracon/commit/cfa4b1a7ea4968c299cef0ae44bc081439aa3c8f))
* **gateway:** ask providers for uncompressed responses so refusals are readable ([#285](https://github.com/cosmicspork/tracon/issues/285)) ([29e33ce](https://github.com/cosmicspork/tracon/commit/29e33ce70ae709db086c16997a9090065ba04235))
* **gateway:** bound model streams by silence, not total time ([#274](https://github.com/cosmicspork/tracon/issues/274)) ([77dfd82](https://github.com/cosmicspork/tracon/commit/77dfd8234c2100ad1bd9a8582e02fa1d80143a26))
* **opencode:** answer the native app's two startup reads instead of refusing them ([#277](https://github.com/cosmicspork/tracon/issues/277)) ([2f04fca](https://github.com/cosmicspork/tracon/commit/2f04fcac29583ae6a5210de32752fc21b48dbdfe))
* **opencode:** give the session runner the node's config, tools and orientation, and refuse an ungated launch ([#281](https://github.com/cosmicspork/tracon/issues/281)) ([c7640ff](https://github.com/cosmicspork/tracon/commit/c7640fffcaf36d55b7ef4298b13a55d08c990b13))
* **opencode:** stage manifest skills where both halves find them, and say what to do about a configured repo ([#282](https://github.com/cosmicspork/tracon/issues/282)) ([c48e9d6](https://github.com/cosmicspork/tracon/commit/c48e9d6e771ada2336d01daaf334efba87592823))
* **opencode:** stage the config directory's .gitignore so the sealed mount is never written ([#283](https://github.com/cosmicspork/tracon/issues/283)) ([7cc66dc](https://github.com/cosmicspork/tracon/commit/7cc66dcb352f03d7f9fbbf0b69a9a583fc85f889))
* **policy:** let a review or report's prose name what a deny rule guards ([#273](https://github.com/cosmicspork/tracon/issues/273)) ([3c3f91c](https://github.com/cosmicspork/tracon/commit/3c3f91c08b0224b8b8f71c80af2a942c9419961a))
* **runner:** leave a staged file mount alone when preparing a Kubernetes pod's volume ([#291](https://github.com/cosmicspork/tracon/issues/291)) ([aab0897](https://github.com/cosmicspork/tracon/commit/aab08971acb24059c870a6fd953631190bde8bbd))
* **session:** end a harness turn on silence, not after twenty minutes of work ([#288](https://github.com/cosmicspork/tracon/issues/288)) ([9070bbf](https://github.com/cosmicspork/tracon/commit/9070bbf384918acc5d64964a57d2c86748d37299))
* **session:** let the node's own MCP tools through the harness's ask; the node decides them when called ([#287](https://github.com/cosmicspork/tracon/issues/287)) ([95baa33](https://github.com/cosmicspork/tracon/commit/95baa332dc6cc284bb53d60556cf4d50304c8478))
* **session:** record a failed step as an error, not a refusal ([#278](https://github.com/cosmicspork/tracon/issues/278)) ([b356551](https://github.com/cosmicspork/tracon/commit/b35655128543de873e2b79aac3aa7031abd446b5))
* **workspace:** overlay a checkout without what its ignore rules exclude ([#275](https://github.com/cosmicspork/tracon/issues/275)) ([5607020](https://github.com/cosmicspork/tracon/commit/5607020d9bdec7adefea72e1be99cc60c8b390c2))

## [0.23.0](https://github.com/cosmicspork/tracon/compare/v0.22.0...v0.23.0) (2026-09-28)


### Features

* **external:** give each connected agent its own session ([#270](https://github.com/cosmicspork/tracon/issues/270)) ([90e6fb7](https://github.com/cosmicspork/tracon/commit/90e6fb7291f969cf0ae52d0e842039179c7b49b3))
* **external:** stop and clear broker access per channel from the GUI and CLI ([#268](https://github.com/cosmicspork/tracon/issues/268)) ([85cae38](https://github.com/cosmicspork/tracon/commit/85cae38929f2e1195e65e1818a015b520b514b48))
* offer a reload when the node runs a newer interface ([#260](https://github.com/cosmicspork/tracon/issues/260)) ([3ce84d4](https://github.com/cosmicspork/tracon/commit/3ce84d4a566920c55a9a43c05f5f8e7a85e3e72a))
* **providers:** choose and edit a sign-in's channels ([#259](https://github.com/cosmicspork/tracon/issues/259)) ([cd553e7](https://github.com/cosmicspork/tracon/commit/cd553e72d521483aa8a8214164fa49e2643d3657))
* reclaim runtime storage whose owner is over ([#266](https://github.com/cosmicspork/tracon/issues/266)) ([46df837](https://github.com/cosmicspork/tracon/commit/46df83733ef0dd8302cba7dfc17f762339d70e5b))
* **review:** raise the submission cap to 10k lines and 200 files ([#269](https://github.com/cosmicspork/tracon/issues/269)) ([1a8be3f](https://github.com/cosmicspork/tracon/commit/1a8be3fee96787b0b84667d6f2bef10177af0763))


### Bug Fixes

* **cli:** accept the short work ids that ls prints ([#261](https://github.com/cosmicspork/tracon/issues/261)) ([823b079](https://github.com/cosmicspork/tracon/commit/823b079d1123501bc179ec0ef3868588beaa892c))
* **cli:** print memory ids in full and accept a unique prefix in rm ([#262](https://github.com/cosmicspork/tracon/issues/262)) ([0453943](https://github.com/cosmicspork/tracon/commit/04539436cc0acedec311d2944d444b87c08178fc))
* **external:** withdraw a card whose caller hung up, and log the call as abandoned ([#272](https://github.com/cosmicspork/tracon/issues/272)) ([3db6dc6](https://github.com/cosmicspork/tracon/commit/3db6dc6acd88f15b6eb82bda327b9536c15687a1))
* **publish:** drop glab's nonexistent --no-squash-before-merge flag ([#271](https://github.com/cosmicspork/tracon/issues/271)) ([33db9bd](https://github.com/cosmicspork/tracon/commit/33db9bd0cb9d6f9b09d61ebb0b6877b1ffc591dc))
* **spa:** grow the review body to fit what it holds ([#265](https://github.com/cosmicspork/tracon/issues/265)) ([cbf7867](https://github.com/cosmicspork/tracon/commit/cbf78674baceeb31155f9013ac857352ff275b6a))
* **spa:** open list screens on the node's default channel ([#257](https://github.com/cosmicspork/tracon/issues/257)) ([e40cdc4](https://github.com/cosmicspork/tracon/commit/e40cdc46cb4da8d96ea0c25481ac72d6d9e6b641))


### Performance Improvements

* **review:** snapshot a candidate with two git processes, not two per file ([#267](https://github.com/cosmicspork/tracon/issues/267)) ([63af708](https://github.com/cosmicspork/tracon/commit/63af708ac81c0b1bde474c20162de3b204e73f8d))

## [0.22.0](https://github.com/cosmicspork/tracon/compare/v0.21.0...v0.22.0) (2026-09-26)


### Features

* **context:** let the operator select what every attempt at an item starts with ([#256](https://github.com/cosmicspork/tracon/issues/256)) ([af0991b](https://github.com/cosmicspork/tracon/commit/af0991bb711ca2a006a991715fb367be996daeae))


### Bug Fixes

* finish first-task onboarding ([#254](https://github.com/cosmicspork/tracon/issues/254)) ([9f0be75](https://github.com/cosmicspork/tracon/commit/9f0be75d4004454c63719aa9cc61b6fd4134695f))

## [0.21.0](https://github.com/cosmicspork/tracon/compare/v0.20.0...v0.21.0) (2026-09-22)


### Features

* **brief:** record who said what a work item is for ([#243](https://github.com/cosmicspork/tracon/issues/243)) ([6ccea84](https://github.com/cosmicspork/tracon/commit/6ccea849b90cd88c209f468a55e31f9d3828aa79))
* pin documents into orientation, make memory promotion editable ([#252](https://github.com/cosmicspork/tracon/issues/252)) ([1b814b6](https://github.com/cosmicspork/tracon/commit/1b814b68c0d3137643ba2bb5159a3f24d06a6015))
* **providers:** declare models from OpenCode's own catalogue, not a hardcoded list ([#250](https://github.com/cosmicspork/tracon/issues/250)) ([8bb15c6](https://github.com/cosmicspork/tracon/commit/8bb15c6e7347a595ae17a6b4bddf28a3f21b35d2))
* **providers:** declare the current models for the built-in providers by default ([#246](https://github.com/cosmicspork/tracon/issues/246)) ([18e0cb0](https://github.com/cosmicspork/tracon/commit/18e0cb08f6f3e7715acc448ba4457680b92d0cb0))
* redesign Connections around an add-a-provider flow ([#251](https://github.com/cosmicspork/tracon/issues/251)) ([62c224b](https://github.com/cosmicspork/tracon/commit/62c224b6c6f90bde3e779e01d3cd173c3344db14))
* **settings:** edit each provider's declared models from the Connections pane ([#247](https://github.com/cosmicspork/tracon/issues/247)) ([63b5114](https://github.com/cosmicspork/tracon/commit/63b51149197d1477ed2bd690c2c40780a2c5806d))


### Bug Fixes

* **harness:** make real sessions start and reach their providers on both harnesses ([#245](https://github.com/cosmicspork/tracon/issues/245)) ([5e8a06a](https://github.com/cosmicspork/tracon/commit/5e8a06a74ae5c96f8d583f1ec94c6c74185ef2a9))
* **orientation:** separate tracon's own orientation from the operator's notes ([#248](https://github.com/cosmicspork/tracon/issues/248)) ([b3c42b1](https://github.com/cosmicspork/tracon/commit/b3c42b16b5652c0db6d72377bf6714aeb9433a68))
* **spa:** show API-key providers in Connections, promoted memories in the browse view ([#253](https://github.com/cosmicspork/tracon/issues/253)) ([b60bbe3](https://github.com/cosmicspork/tracon/commit/b60bbe300509affa2ce1f3b4af9892967d24b37c))

## [0.20.0](https://github.com/cosmicspork/tracon/compare/v0.19.0...v0.20.0) (2026-09-18)


### Features

* **spa:** re-probe the model catalogue, counted per provider ([#241](https://github.com/cosmicspork/tracon/issues/241)) ([c71cb61](https://github.com/cosmicspork/tracon/commit/c71cb61684c499118e443c4a576adad1d908ecfd))

## [0.19.0](https://github.com/cosmicspork/tracon/compare/v0.18.0...v0.19.0) (2026-09-17)


### Features

* **attention:** count actionable decisions, and say what each request is for ([#239](https://github.com/cosmicspork/tracon/issues/239)) ([b60b87f](https://github.com/cosmicspork/tracon/commit/b60b87ff06aad411b2db6819916ee08ab3c180c6))
* **authority:** let the gate explain itself, in the terms the task is in ([#240](https://github.com/cosmicspork/tracon/issues/240)) ([f9343bc](https://github.com/cosmicspork/tracon/commit/f9343bcc72132faca8bce19694ab847f6e4fba82))
* **orientation:** reserve a session's context for its task before its guides ([#236](https://github.com/cosmicspork/tracon/issues/236)) ([47fb192](https://github.com/cosmicspork/tracon/commit/47fb192b20fd1bf8e74a5732049e90a4b4fa7c20))
* **review:** bind every verdict to the revision the operator inspected ([#234](https://github.com/cosmicspork/tracon/issues/234)) ([3286f5b](https://github.com/cosmicspork/tracon/commit/3286f5b2fbaab832bef9b33e956b1959811d3772))


### Bug Fixes

* normalize managed harness permissions ([#238](https://github.com/cosmicspork/tracon/issues/238)) ([29c4134](https://github.com/cosmicspork/tracon/commit/29c413428683ceb30c2ac3efd2db966679e71cad))

## [0.18.0](https://github.com/cosmicspork/tracon/compare/v0.17.0...v0.18.0) (2026-09-15)


### Features

* **mesh:** sign in once for the mesh and renew shared credentials by claim ([#233](https://github.com/cosmicspork/tracon/issues/233)) ([6cc0747](https://github.com/cosmicspork/tracon/commit/6cc07477ec38459604055a3c97ed26492312aae3))
* **providers:** sign in to Anthropic and ChatGPT subscriptions natively ([#230](https://github.com/cosmicspork/tracon/issues/230)) ([cb23cb5](https://github.com/cosmicspork/tracon/commit/cb23cb5e0f546aa5642b1254f45eeac94381a3fc))

## [0.17.0](https://github.com/cosmicspork/tracon/compare/v0.16.2...v0.17.0) (2026-09-15)


### Features

* group settings into cards and move desktop preferences into Settings ([#227](https://github.com/cosmicspork/tracon/issues/227)) ([fa65008](https://github.com/cosmicspork/tracon/commit/fa650082722a33a411733c2f2b54836c1820d4c6))


### Bug Fixes

* **providers:** submit the pasted Anthropic code, sign in to Codex by device code, hide API-key-only cards ([#228](https://github.com/cosmicspork/tracon/issues/228)) ([474c4c8](https://github.com/cosmicspork/tracon/commit/474c4c8f8ee8b51c1ffad8e73f671622c60730e3))

## [0.16.2](https://github.com/cosmicspork/tracon/compare/v0.16.1...v0.16.2) (2026-09-15)


### Bug Fixes

* stop forcing Secure on the loopback operator cookie ([#225](https://github.com/cosmicspork/tracon/issues/225)) ([377aeda](https://github.com/cosmicspork/tracon/commit/377aedaa2d006e7379ca2e3009328e4ada336aaa))

## [0.16.1](https://github.com/cosmicspork/tracon/compare/v0.16.0...v0.16.1) (2026-09-14)


### Bug Fixes

* **desktop:** migrate an omp node.toml and surface a failing node service ([#223](https://github.com/cosmicspork/tracon/issues/223)) ([566b018](https://github.com/cosmicspork/tracon/commit/566b01863383b40e145fb5f812ee07e3b5cf1803))
* **desktop:** remove Apple code signing from macOS updates and releases ([#222](https://github.com/cosmicspork/tracon/issues/222)) ([419d971](https://github.com/cosmicspork/tracon/commit/419d9716e6c73daa42b5d0f9cfbb10a67283c029))

## [0.16.0](https://github.com/cosmicspork/tracon/compare/v0.15.1...v0.16.0) (2026-09-14)


### ⚠ BREAKING CHANGES

* the omp harness is gone. `[harness] id = "omp"` is refused at startup with the migration path; its adapter, image, release job, ACP stdio layer, provider wiring and catalogue denylist are removed. Sessions it ran are archived read-only by `tracon session archive-legacy`, keeping their harness identity, transcripts, evidence and workspaces, and are carried forward with `tracon session reopen <id> --harness opencode`.

### Features

* **adapter:** drive OpenCode v1.18.30 through its server API with a sealed launch environment ([#193](https://github.com/cosmicspork/tracon/issues/193)) ([6e1695c](https://github.com/cosmicspork/tracon/commit/6e1695c1bc16c597981e1347a411e33f8ed721bb))
* **desktop:** an unprivileged window for the OpenCode UI, navigable only to its origin ([#205](https://github.com/cosmicspork/tracon/issues/205)) ([8b85729](https://github.com/cosmicspork/tracon/commit/8b85729e4281d36ef53e2b11bb09264c0c050fe5))
* **gateway:** mediate OpenCode's native API per session, deny by default, ask-only permissions ([#195](https://github.com/cosmicspork/tracon/issues/195)) ([192f910](https://github.com/cosmicspork/tracon/commit/192f9102e398a0df0b7fa886b817e87cf2c48e05))
* **gateway:** PTY as an explicit terminal capability with owner-bound tickets and a bounded proxy ([#207](https://github.com/cosmicspork/tracon/issues/207)) ([94e8429](https://github.com/cosmicspork/tracon/commit/94e8429de4b22d5d09d905d43995a74d47e1c538))
* **http:** serve OpenCode's native UI from its own origin behind a single-use bootstrap ([#206](https://github.com/cosmicspork/tracon/issues/206)) ([8eefbd4](https://github.com/cosmicspork/tracon/commit/8eefbd4a830c1a5968d765913fef99d3888be7ab))
* **mesh:** bounded encrypted owner streams over the hub for remote sessions ([#215](https://github.com/cosmicspork/tracon/issues/215)) ([21bab7c](https://github.com/cosmicspork/tracon/commit/21bab7c8ea5376dc93e01afeaadc01b8ef077abe))
* **operator:** unify administration and first-task workflows ([#208](https://github.com/cosmicspork/tracon/issues/208)) ([9b3fe1a](https://github.com/cosmicspork/tracon/commit/9b3fe1aa883d440daf5f12d4864a858ef9d720eb))
* **providers:** log in to an Anthropic subscription through claude setup-token ([#190](https://github.com/cosmicspork/tracon/issues/190)) ([15868ca](https://github.com/cosmicspork/tracon/commit/15868ca0eabc87d8c684d49502c2d8100f7aa119))
* **qa:** deploy candidates to a QA target through a brokered command, for Laravel Cloud and the like ([#218](https://github.com/cosmicspork/tracon/issues/218)) ([9f73d8f](https://github.com/cosmicspork/tracon/commit/9f73d8f7609c641551cb1aeb35d813992b022f39))
* retire the omp harness — OpenCode primary, Claude Code retained, legacy sessions archived ([#216](https://github.com/cosmicspork/tracon/issues/216)) ([069cfb1](https://github.com/cosmicspork/tracon/commit/069cfb17efd701328617f054450aea8798152b8b))
* **runtime:** bake LSP, formatters, and the plugin cache; reject egress; reap harness children ([#202](https://github.com/cosmicspork/tracon/issues/202)) ([9ab28cf](https://github.com/cosmicspork/tracon/commit/9ab28cfeeaaa24d93b595577ae68c8a0f9e92818))
* **session:** a node-owned launch manifest for skills, prompts, and approved plugins ([#203](https://github.com/cosmicspork/tracon/issues/203)) ([456a0ee](https://github.com/cosmicspork/tracon/commit/456a0ee596b69a76b8812fb127fca09606fe8661))
* **session:** durable OpenCode identity mapping, idempotent ingestion, and reconciliation ([#196](https://github.com/cosmicspork/tracon/issues/196)) ([244bbbd](https://github.com/cosmicspork/tracon/commit/244bbbd981f7639a3444e0dd73a4100f6bb7329a))
* **session:** quiesced backups, migration on a clone, and generation-gated restore for OpenCode state ([#204](https://github.com/cosmicspork/tracon/issues/204)) ([705bfc1](https://github.com/cosmicspork/tracon/commit/705bfc1ee3ebdafe101ac6f5c13663a9e1bb30b0))
* **session:** reconcile harness and gateway usage per turn and keep unsent prompts on the node ([#197](https://github.com/cosmicspork/tracon/issues/197)) ([dcd7280](https://github.com/cosmicspork/tracon/commit/dcd7280f3d2bf5040ee724b0ce654708af0f4b03))
* **spa:** keep the native OpenCode view inside the installed app ([#210](https://github.com/cosmicspork/tracon/issues/210)) ([b3038af](https://github.com/cosmicspork/tracon/commit/b3038af7dc648f82a94d478ae0fc5cfe1922c542))
* **ui:** capture the native UI route trace, prove unknown mutations fail closed, and install the pinned bundle ([#211](https://github.com/cosmicspork/tracon/issues/211)) ([26cc5e5](https://github.com/cosmicspork/tracon/commit/26cc5e5b6c615b9494dadb2096be5293e4bd7e29))


### Bug Fixes

* **gateway:** adversarial run against the mediated OpenCode API and per-session state isolation ([#198](https://github.com/cosmicspork/tracon/issues/198)) ([7a7053a](https://github.com/cosmicspork/tracon/commit/7a7053ae6efaa00520fb59b710d8b73b3b6285e6))
* **gateway:** prove OpenCode provider traffic stays inside the gateway allowlist and is counted ([#199](https://github.com/cosmicspork/tracon/issues/199)) ([9014e8a](https://github.com/cosmicspork/tracon/commit/9014e8aea11927c3a9d2df30cf00ce8b1473b52a))
* **gateway:** synthesise a session-scoped global event stream so the native UI sees permissions ([#217](https://github.com/cosmicspork/tracon/issues/217)) ([4e6ebbf](https://github.com/cosmicspork/tracon/commit/4e6ebbff52ba569426342aea7a47b25fa5202f73))
* **providers:** make the IPv6 collision test independent of macOS socket teardown timing ([#220](https://github.com/cosmicspork/tracon/issues/220)) ([d805d79](https://github.com/cosmicspork/tracon/commit/d805d79d87c4cc6e6cfdef05261562ed2abd8899))
* **providers:** stop the callback listener tests racing on loopback ports ([#194](https://github.com/cosmicspork/tracon/issues/194)) ([ee87a32](https://github.com/cosmicspork/tracon/commit/ee87a32bf458b213d75b17d63f5b53f09570cc27))
* **release:** ship an unsigned macOS bundle when Apple credentials are absent ([#192](https://github.com/cosmicspork/tracon/issues/192)) ([0956429](https://github.com/cosmicspork/tracon/commit/0956429bd9463533f44f91f017fb43376909d425))

## [0.15.1](https://github.com/cosmicspork/tracon/compare/v0.15.0...v0.15.1) (2026-09-13)


### Bug Fixes

* **adapter:** pin harness versions, record per-session compatibility, and refuse unsupported protocols ([#187](https://github.com/cosmicspork/tracon/issues/187)) ([0efd8e7](https://github.com/cosmicspork/tracon/commit/0efd8e79dc26e7785877ae381632b82f769bd09b))
* **desktop:** verify process identity before adoption and signalling ([#177](https://github.com/cosmicspork/tracon/issues/177)) ([dff9689](https://github.com/cosmicspork/tracon/commit/dff968920cd75f315b9914ccddb5e7972283b534))
* **enroll:** authenticate the signing and encryption key binding before handing off channel keys ([#179](https://github.com/cosmicspork/tracon/issues/179)) ([9162b21](https://github.com/cosmicspork/tracon/commit/9162b2194bca9f05debf8cbee82556791ce688dd))
* **gateway:** allow only inference methods and paths through the credentialed proxy ([#182](https://github.com/cosmicspork/tracon/issues/182)) ([6babb42](https://github.com/cosmicspork/tracon/commit/6babb42069a0a3869bc78396dd3910d616c2ae3d))
* **http:** reject null-origin operator requests and preflight explicit models on create ([#176](https://github.com/cosmicspork/tracon/issues/176)) ([ea0c0e1](https://github.com/cosmicspork/tracon/commit/ea0c0e149d5b1292a0a040d507bd94e678ad6f61))
* **providers:** make the credential lift and callback listener tests deterministic ([#183](https://github.com/cosmicspork/tracon/issues/183)) ([b88c000](https://github.com/cosmicspork/tracon/commit/b88c000227db4c27ca2d05d0eff2c3cf8e547d3f))
* **publish:** recover interrupted publication idempotently and broker every network git credential ([#184](https://github.com/cosmicspork/tracon/issues/184)) ([4d23054](https://github.com/cosmicspork/tracon/commit/4d23054c9a66fff4fae94c4b0970a3eb3061bb85))
* **review:** bind prototype builds and every check record to the captured candidate tree ([#188](https://github.com/cosmicspork/tracon/issues/188)) ([74df5cc](https://github.com/cosmicspork/tracon/commit/74df5cc9cb4188c8bf805a651e3697a0ee2159b3))
* **session:** guard terminal states against cancellation races, stalled startup, and late completions ([#185](https://github.com/cosmicspork/tracon/issues/185)) ([2b1baab](https://github.com/cosmicspork/tracon/commit/2b1baab09d449f6b5eb9adb984bd233a79351c6b))
* **session:** surface repetition as a signal and keep reports and notifications from pausing or asking ([#186](https://github.com/cosmicspork/tracon/issues/186)) ([e95a62b](https://github.com/cosmicspork/tracon/commit/e95a62b8520b431324752532e221313a93863214))
* **workspace:** harden Git metadata handling in captured and published trees ([#180](https://github.com/cosmicspork/tracon/issues/180)) ([aad3342](https://github.com/cosmicspork/tracon/commit/aad3342b3d0b7a5a8f7dff91a6f3dd112af4676c))

## [0.15.0](https://github.com/cosmicspork/tracon/compare/v0.14.0...v0.15.0) (2026-09-12)


### Features

* **boundary:** images carry a definitions digest so stale ones rebuild ([2e32a59](https://github.com/cosmicspork/tracon/commit/2e32a5907d5624c55b991c9b171edc9cdbdb6ddc))
* **boundary:** images carry a definitions digest so stale ones rebuild ([2a8afa3](https://github.com/cosmicspork/tracon/commit/2a8afa393b6bc15fd86adcbbb8a5ec4281502ccd))
* candidate-bound QA verification and repository-derived prototypes ([#162](https://github.com/cosmicspork/tracon/issues/162)) ([d27c54d](https://github.com/cosmicspork/tracon/commit/d27c54d490f9056b737c50ec0ee2e4d594f614ca))
* continuity transfers and optional hub rollups ([#161](https://github.com/cosmicspork/tracon/issues/161)) ([5d5bd52](https://github.com/cosmicspork/tracon/commit/5d5bd5278670db1be4252569de06c2301a8af60b))
* **corpus:** add HTML documents and interactive previews ([fb68a63](https://github.com/cosmicspork/tracon/commit/fb68a634a25072b369d8e514f466406d6699c391))
* **desktop:** after an app update the CLI and the service move to the new node ([b5f7b8b](https://github.com/cosmicspork/tracon/commit/b5f7b8bd250514feff847b2a1f28219cf4d9991a))
* **desktop:** after an app update the CLI and the service move to the new node ([92391c1](https://github.com/cosmicspork/tracon/commit/92391c1e093b49ef37694aa8d7a61ef6094d62ed))
* **desktop:** the node runs under the user service; the app installs and manages it ([9ddcd20](https://github.com/cosmicspork/tracon/commit/9ddcd206dc80c51c7d26d7c85a206cd337949e71))
* **desktop:** the node runs under the user service; the app installs and manages it ([989d65e](https://github.com/cosmicspork/tracon/commit/989d65e5461a5aa75253a5c93c10f23696d2afae))
* **docs:** archived documents ([e0ce6f9](https://github.com/cosmicspork/tracon/commit/e0ce6f9f27e63f2f99937317a60cc81563a7a017))
* **docs:** archived documents ([92aea75](https://github.com/cosmicspork/tracon/commit/92aea7582af2c67091623d851ee0a6dd9a1adbfc))
* **docs:** export the corpus to a directory on a timer ([fdcb458](https://github.com/cosmicspork/tracon/commit/fdcb458b5c12ffe82d1eb750c057f5c175afd06f))
* **docs:** export the corpus to a directory on a timer ([12e3e43](https://github.com/cosmicspork/tracon/commit/12e3e4330fd65cfdd189f35a04ea9cfb3bbcce2f))
* immutable candidate evidence for review ([#160](https://github.com/cosmicspork/tracon/issues/160)) ([4269178](https://github.com/cosmicspork/tracon/commit/426917819f3b30e9cdfc7320116e9a03b746ae1c))
* **metrics:** measure time to verified work, setup failures, and waiting ([#155](https://github.com/cosmicspork/tracon/issues/155)) ([22d55a7](https://github.com/cosmicspork/tracon/commit/22d55a7ee94b8d7a996ad9982c5814c70af819ee))
* **onboarding:** put local runtime readiness first ([#153](https://github.com/cosmicspork/tracon/issues/153)) ([fdc585e](https://github.com/cosmicspork/tracon/commit/fdc585e6806d50ad5740a6ac9aac9cf97b1b35f4))
* operator intervention tools ([#154](https://github.com/cosmicspork/tracon/issues/154)) ([b42b5b6](https://github.com/cosmicspork/tracon/commit/b42b5b6adef035edc441041d7172bf681eb97429))
* optional workflow and real pause/stop controls ([#159](https://github.com/cosmicspork/tracon/issues/159)) ([ae258ef](https://github.com/cosmicspork/tracon/commit/ae258ef921b8d8fbc5aec42d5be4ffe6e763a8a0))
* **permissions:** edit a brokered tool call's arguments on its card ([08f1350](https://github.com/cosmicspork/tracon/commit/08f1350aa5755d93c4a4d3442a676b6fdaf430ad))
* **permissions:** edit a brokered tool call's arguments on its card ([a3d26f3](https://github.com/cosmicspork/tracon/commit/a3d26f38d2def6563e373a59161e5ff8854a6d2b))
* **policy:** argument-scoped allow rules; bundle v6 ([15427ce](https://github.com/cosmicspork/tracon/commit/15427ce807675bd68ecfe286f058fecb607097c3))
* **policy:** argument-scoped allow rules; bundle v6 ([769bae7](https://github.com/cosmicspork/tracon/commit/769bae705f7eb39f6d319bf4bbb5ecf431deabab))
* provenance-verified releases and paginated forge listings ([#157](https://github.com/cosmicspork/tracon/issues/157)) ([0f3bf6c](https://github.com/cosmicspork/tracon/commit/0f3bf6ca5d24674ddd28217bb7c4c38c87e9e37b))
* **review:** an external harness submits its own worktree for review ([8a1cc42](https://github.com/cosmicspork/tracon/commit/8a1cc4245096a3ac908b9944413b35e27549bbca))
* **review:** an external harness submits its own worktree for review ([b9f9b4d](https://github.com/cosmicspork/tracon/commit/b9f9b4da2c0926e6077a3f79b07aaeab3da9a71c))
* runtime-owned workspaces without host bind mounts ([#158](https://github.com/cosmicspork/tracon/issues/158)) ([f02995d](https://github.com/cosmicspork/tracon/commit/f02995d80bb5047c9d1fc0f2fdae793fc7067eb0))
* scoped authority grants for consequential actions ([#156](https://github.com/cosmicspork/tracon/issues/156)) ([88a6c72](https://github.com/cosmicspork/tracon/commit/88a6c7221cf70e3cc6cb04da637488249a0542a4))
* **service:** the unit runs the binary that installed it; the node starts the podman machine ([fc2c579](https://github.com/cosmicspork/tracon/commit/fc2c579a14a808bfc61cff5f9755a24df784fe8b))
* **service:** the unit runs the binary that installed it; the node starts the podman machine ([db637cb](https://github.com/cosmicspork/tracon/commit/db637cbd76b05757bb15b74399c1c8f40db42540))
* **tools:** consulta profiles within a channel ([a7f707e](https://github.com/cosmicspork/tracon/commit/a7f707eb361798f8978436b1317fa85442022139))
* **tools:** consulta profiles within a channel ([bbe7e74](https://github.com/cosmicspork/tracon/commit/bbe7e741e0e15abb38ad83d3fb4a1ec608d66cb7))
* **tools:** github pr_status, pr_comment, run_status ([829b9cf](https://github.com/cosmicspork/tracon/commit/829b9cfec5edd4e9623e79cb2dd0946f23cd3d59))
* **tools:** github pr_status, pr_comment, run_status ([7f43ff9](https://github.com/cosmicspork/tracon/commit/7f43ff92ad01c1a3c0e8273a2c50c39cd455d036))
* **tools:** jira issue search and gitlab pipelines ([7660a03](https://github.com/cosmicspork/tracon/commit/7660a03bc489e4a9bad32a125eecb06fb9ae5439))
* **tools:** jira issue search and gitlab pipelines ([115f9ab](https://github.com/cosmicspork/tracon/commit/115f9abfabbfc6261d153807404b43e0d39ecce0))


### Bug Fixes

* **ci:** release test database lock before await ([23e495b](https://github.com/cosmicspork/tracon/commit/23e495b82bd7fe31c0f5cd81a1dcf7e5f98fda51))
* **gateway:** shape Anthropic subscription requests as the token demands ([#166](https://github.com/cosmicspork/tracon/issues/166)) ([cab972b](https://github.com/cosmicspork/tracon/commit/cab972bd818cf8d9ea129b4a108706327cdaaade))
* **omp:** list Codex models under openai-codex in the probed catalogue ([#174](https://github.com/cosmicspork/tracon/issues/174)) ([21b42c3](https://github.com/cosmicspork/tracon/commit/21b42c3dcdcc3b61bd0c0ba3eb13f27fc45dd2e9))
* **omp:** pass the servable predicate in the harness wiring tests ([#175](https://github.com/cosmicspork/tracon/issues/175)) ([267f048](https://github.com/cosmicspork/tracon/commit/267f048ba529112b54307bca88508fb6785dc0c2))
* **omp:** pin models.yml so a probe's token cannot shadow a session's ([#165](https://github.com/cosmicspork/tracon/issues/165)) ([3531835](https://github.com/cosmicspork/tracon/commit/3531835b87fc382e162d4f67dc5a6be7a0935837))
* **omp:** surface provider errors during a turn and skip unbound local probes ([#173](https://github.com/cosmicspork/tracon/issues/173)) ([e7fe7fc](https://github.com/cosmicspork/tracon/commit/e7fe7fc6a7f818f22a98da783b427442cd7b7da0))
* **review:** cap review_status wait below the MCP client timeout ([#172](https://github.com/cosmicspork/tracon/issues/172)) ([ff3b864](https://github.com/cosmicspork/tracon/commit/ff3b8641f6b03b8fe6e19720132b4b0e2e87186f))
* **review:** persist and return the check command in check_run evidence ([#171](https://github.com/cosmicspork/tracon/issues/171)) ([facf126](https://github.com/cosmicspork/tracon/commit/facf126c1c08d9d0789eca71e1f2f836d9f9b1ea))

## [0.14.0](https://github.com/cosmicspork/tracon/compare/v0.13.1...v0.14.0) (2026-09-09)


### Features

* **channels:** a default channel, per node and per browser ([169f8dc](https://github.com/cosmicspork/tracon/commit/169f8dcb9bf51ad88bb2294ba4ed365fbed0c0cf))
* **channels:** a default channel, per node and per browser ([61416d6](https://github.com/cosmicspork/tracon/commit/61416d667650ef9a1d18a0428e7130e0e131e3f0))
* **channels:** delete an archived channel from this node ([7a4a0f4](https://github.com/cosmicspork/tracon/commit/7a4a0f45bd5b4a09398d550055c34cb9f6c6d8c7))
* **channels:** delete an archived channel from this node ([c6bc1ca](https://github.com/cosmicspork/tracon/commit/c6bc1ca532271f22b9927c2f4995aba614282b0d))
* **mesh:** the hub as a card, and a way to unpair ([14a5b4c](https://github.com/cosmicspork/tracon/commit/14a5b4c2fadbbd40d759dab9e9234da1870aed4f))
* **mesh:** the hub as a card, and a way to unpair ([6fd8e8c](https://github.com/cosmicspork/tracon/commit/6fd8e8c77a4e866fc9bc81ab383f806cdf200fd4))
* **spa:** search boxes for repositories and models, a budget that reads ([710b9d0](https://github.com/cosmicspork/tracon/commit/710b9d02ee72ba93d7367b49c7681251bff7747d))
* **spa:** search boxes for repositories and models, a budget that reads ([a8a599d](https://github.com/cosmicspork/tracon/commit/a8a599d12e83cc45972880cffe182a15ead85a52))
* **wrapper:** keep the node running through an app update ([12f8272](https://github.com/cosmicspork/tracon/commit/12f82726ed6cc9d8ebe01bd6c4df0086077a0410))
* **wrapper:** keep the node running through an app update ([5e8e52d](https://github.com/cosmicspork/tracon/commit/5e8e52d27cdffe13c46c824101998d00ee859466))


### Bug Fixes

* **forge:** clone GitLab projects under their group path, say when a token is rejected ([d30c560](https://github.com/cosmicspork/tracon/commit/d30c56081ef80ed6dd340c9773df2ba0eec99773))
* **forge:** clone GitLab projects under their group path, say when a token is rejected ([602b34c](https://github.com/cosmicspork/tracon/commit/602b34ccf9f4e6e1de5a1a9e4a861b793c9bbac9))
* **spa:** keep the loopback flag across node frames, fold node cards, trim the text ([06655dd](https://github.com/cosmicspork/tracon/commit/06655dd492ed2782b4ecfc97becd0ebb0aa717c4))
* **spa:** keep the loopback flag across node frames, fold node cards, trim the text ([f836827](https://github.com/cosmicspork/tracon/commit/f8368274586510f65f7a71e2b3edd2776107e6fc))

## [0.13.1](https://github.com/cosmicspork/tracon/compare/v0.13.0...v0.13.1) (2026-09-08)


### Bug Fixes

* **node:** an attached harness's card reaches an interface already open ([#125](https://github.com/cosmicspork/tracon/issues/125)) ([83e1598](https://github.com/cosmicspork/tracon/commit/83e159815ad72e7e60e51258f1588260cca54985))
* **wrapper:** a check that never answers must not disable the update button ([#124](https://github.com/cosmicspork/tracon/issues/124)) ([625ce90](https://github.com/cosmicspork/tracon/commit/625ce90fd03d496e18dbcf1372bd79f5cc857ed1))

## [0.13.0](https://github.com/cosmicspork/tracon/compare/v0.12.2...v0.13.0) (2026-09-08)


### Features

* **node:** a harness you run yourself, using this node's tools ([a5728c3](https://github.com/cosmicspork/tracon/commit/a5728c33dddae1d6c58719efea25246606aa41bc))


### Bug Fixes

* **mesh:** read the enrolment backlog without a poll interval per page ([#121](https://github.com/cosmicspork/tracon/issues/121)) ([e744f65](https://github.com/cosmicspork/tracon/commit/e744f6520801baeddbc2b3837d120d8501a40c7a))
* **spa:** keep the snapshots that answered when one endpoint fails ([#122](https://github.com/cosmicspork/tracon/issues/122)) ([a8872f8](https://github.com/cosmicspork/tracon/commit/a8872f892883129ededeedfab4a629ae8363f24f))

## [0.12.2](https://github.com/cosmicspork/tracon/compare/v0.12.1...v0.12.2) (2026-09-03)


### Bug Fixes

* **node:** compile remote device login ([d2bbfde](https://github.com/cosmicspork/tracon/commit/d2bbfde163854aa31985c6b5ce2d5b22d4ba51af))
* **node:** use device login remotely ([9fd9da4](https://github.com/cosmicspork/tracon/commit/9fd9da42b2ad27df8a694135210986be9ef35db9))
* **node:** use device login remotely ([15c5ad2](https://github.com/cosmicspork/tracon/commit/15c5ad2b09c3bd225f7cab0dc2b5df5d7b96bab7))
* **spa:** defer provider sign-in window ([9c31a05](https://github.com/cosmicspork/tracon/commit/9c31a0563de87dfd2f5625345da1fa0d06e4deac))
* **spa:** defer provider sign-in window ([960ce0d](https://github.com/cosmicspork/tracon/commit/960ce0d16cfdee7558300c811eb68b18faaae313))

## [0.12.1](https://github.com/cosmicspork/tracon/compare/v0.12.0...v0.12.1) (2026-09-03)


### Bug Fixes

* **spa:** preserve provider popup navigation in PWAs ([f568a12](https://github.com/cosmicspork/tracon/commit/f568a126ca91371e674f1ad644cd63bc30560609))
* **spa:** preserve provider popup navigation in PWAs ([e446a2c](https://github.com/cosmicspork/tracon/commit/e446a2c30ad1e78b0598c295dad02deacbd23129))

## [0.12.0](https://github.com/cosmicspork/tracon/compare/v0.11.0...v0.12.0) (2026-09-03)


### Features

* make connection setup work from the GUI ([#112](https://github.com/cosmicspork/tracon/issues/112)) ([65253f3](https://github.com/cosmicspork/tracon/commit/65253f33eb95329e4137ed191dacd905cbfc140e))

## [0.11.0](https://github.com/cosmicspork/tracon/compare/v0.10.0...v0.11.0) (2026-09-02)


### Features

* a channel binds a model to each phase ([#106](https://github.com/cosmicspork/tracon/issues/106)) ([c3fe562](https://github.com/cosmicspork/tracon/commit/c3fe562c99746749598a39bb147f65a0dc46e73b))
* one home, and work starts by typing what needs doing ([#107](https://github.com/cosmicspork/tracon/issues/107)) ([a9af969](https://github.com/cosmicspork/tracon/commit/a9af9695d5cd0b08eaace14c3887e13447d35b00))
* put sessions and channels away without losing them ([#108](https://github.com/cosmicspork/tracon/issues/108)) ([7c92da6](https://github.com/cosmicspork/tracon/commit/7c92da657a99abf42813c830326a96ab63d39db9))
* **spa:** a hub you can pair, from the words that said you had none ([#109](https://github.com/cosmicspork/tracon/issues/109)) ([2c3e3a4](https://github.com/cosmicspork/tracon/commit/2c3e3a4a9443606773ffa7aef21e9121149d723d))
* **wrapper:** add macOS self-update ([#103](https://github.com/cosmicspork/tracon/issues/103)) ([b2b0931](https://github.com/cosmicspork/tracon/commit/b2b09317f3d21d134eb9a32a84430579bbdb7b40))


### Bug Fixes

* **node:** write the identity seed atomically ([#111](https://github.com/cosmicspork/tracon/issues/111)) ([1843a6c](https://github.com/cosmicspork/tracon/commit/1843a6c8c325176c38b65cbe34558a9b1dda9463))
* **spa:** the channel a node actually has, repos by name, errors in words ([#105](https://github.com/cosmicspork/tracon/issues/105)) ([3205d41](https://github.com/cosmicspork/tracon/commit/3205d418ac48d2c0b5a525c7a15a16618480578d))

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
