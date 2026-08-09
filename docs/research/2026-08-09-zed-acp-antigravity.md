# Zed, ACP, Gemini CLI, and Google Antigravity

**Research snapshot:** 2026-08-09 (America/Chicago)

**Scope:** current public behavior, distribution, protocol contracts, and implications for
Marion's native-harness facade and ACP client.

**Source policy:** only official Zed, Agent Client Protocol (ACP), and Google documentation,
source repositories, schemas, announcements, releases, and terms were used. Counts, versions,
and product-access statements are date-sensitive.

## Executive conclusion

1. **Zed does not bundle a bespoke integration for every agent it displays.** It combines one
   ACP client/runtime, a registry-backed installer, custom local agent-server configuration, and
   a separate native terminal-thread path. On this snapshot, Zed's ACP directory enumerates 44
   agent cards while the live ACP registry JSON contains 38 entries; the difference reflects
   native, registry, and adapter routes, not 44 agents compiled into Zed
   ([Zed ACP directory](https://zed.dev/acp),
   [live registry JSON](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json)).
2. **“Gemini CLI was replaced/discontinued” is too broad.** Google ended consumer Google-account
   requests from Gemini CLI for free individual, AI Pro, and AI Ultra users on 2026-06-18 and
   directed those users to Antigravity CLI. Enterprise Code Assist and API-key access were
   explicitly unaffected. Gemini CLI remains an actively distributed Apache-2.0 project and its
   current `gemini --acp` mode is the verified Google ACP surface
   ([transition announcement](https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/),
   [cutoff announcement](https://github.com/google-gemini/gemini-cli/discussions/28017),
   [Gemini CLI repository](https://github.com/google-gemini/gemini-cli),
   [ACP-mode documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md)).
3. **Antigravity CLI is a different product surface, not a verified ACP successor.** The `agy`
   TUI shares Antigravity 2.0's harness and now has a one-shot NDJSON output mode, but the reviewed
   first-party documentation and changelog do not document ACP. Its public repository distributes
   compiled releases but does not publish the CLI implementation or an open-source license
   ([CLI launch](https://antigravity.google/blog/introducing-google-antigravity-cli),
   [CLI overview](https://antigravity.google/docs/cli/overview),
   [CLI changelog](https://antigravity.google/changelog?tab=cli),
   [official repository](https://github.com/google-antigravity/antigravity-cli)).
4. **Marion should retain two first-class lanes:** transparent PTY facades for exact native UX,
   and protocol-native structured drivers for ACP. Share package resolution, provenance, policy,
   and normalized capabilities across the lanes, but do not pretend a PTY or NDJSON event stream
   implements ACP semantics.
5. **Do not ship consumer Antigravity automation now.** Antigravity's consumer terms prohibit
   third-party tools from accessing the service and give third-party OAuth automation as an
   example. Treat `agy` wrapping as legally blocked until Google supplies an authorized interface
   or written clarification; a transparent PTY does not remove that product/terms risk
   ([Antigravity terms, §6](https://antigravity.google/terms)).

## 1. What Zed actually implements

### 1.1 Installation and discovery

**Verified facts**

- Zed exposes registry agents through the Agent Panel's new-thread selector. A user can browse
  the registry through the `zed: acp registry` action or configure an agent in settings. Zed also
  accepts custom `agent_servers` entries with a local `command`, `args`, and `env`
  ([External Agents](https://zed.dev/docs/ai/external-agents)).
- Agent-server extensions are deprecated as of extension API v1.5.0. Zed documents registry
  packages as the replacement distribution path and says previously installed agent extensions
  are migrated to their registry equivalents when possible
  ([Agent Server Extensions](https://zed.dev/docs/extensions/agent-servers),
  [ACP Registry announcement](https://zed.dev/blog/acp-registry)).
- An ACP registry manifest describes identity, semantic version, description, optional
  provenance/icon, and a distribution recipe. The schema supports platform binaries, `npx`, and
  `uvx`; binary recipes may declare command arguments, environment variables, archive layout,
  and SHA-256 checksums. It intentionally does **not** make static capability or authentication
  claims; those are negotiated with the running process
  ([registry RFD](https://agentclientprotocol.com/rfds/acp-agent-registry),
  [registry schema](https://raw.githubusercontent.com/agentclientprotocol/registry/main/agent.schema.json)).
- The registry repository validates schema conformance, IDs, URLs, icons, authentication
  handshake behavior, and platform coverage; its CDN `latest` projection is refreshed hourly
  ([registry repository](https://github.com/agentclientprotocol/registry)).
- Zed's current source keeps a canonical registry URL and disk cache, refreshes it periodically,
  selects a compatible distribution, installs versioned artifacts on demand, and reports updates
  ([registry store](https://raw.githubusercontent.com/zed-industries/zed/main/crates/project/src/agent_registry_store.rs),
  [agent-server store](https://raw.githubusercontent.com/zed-industries/zed/main/crates/project/src/agent_server_store.rs)).
- Zed verifies a declared SHA-256 or a GitHub release-asset digest when one is available. The
  registry schema does not require every binary artifact to carry a digest, so registry presence
  alone is not a complete supply-chain guarantee
  ([agent-server store](https://raw.githubusercontent.com/zed-industries/zed/main/crates/project/src/agent_server_store.rs),
  [registry schema](https://raw.githubusercontent.com/agentclientprotocol/registry/main/agent.schema.json)).

**Date-sensitive source observation:** the inspected Zed `main` installation path explicitly
handles binary and `npx` distributions, while the registry schema also permits `uvx`. This is a
current client-implementation gap, not an ACP limitation; Marion should report an unsupported
distribution explicitly rather than silently skipping it
([agent-server store](https://raw.githubusercontent.com/zed-industries/zed/main/crates/project/src/agent_server_store.rs),
[registry schema](https://raw.githubusercontent.com/agentclientprotocol/registry/main/agent.schema.json)).

### 1.2 Runtime architecture and ownership

Zed launches an agent as a subprocess on demand. ACP v1 uses newline-delimited UTF-8 JSON-RPC over
stdin/stdout, with stderr reserved for logs; one connection can serve multiple sessions and both
sides can issue requests and notifications
([ACP architecture](https://agentclientprotocol.com/get-started/architecture),
[ACP transports](https://agentclientprotocol.com/protocol/v1/transports)).

| Layer | Zed/client owns | Agent process owns |
|---|---|---|
| Distribution | registry browsing, resolution, download/cache, launch | publishing its package and executable |
| Conversation | thread UI/history, diff and tool-call presentation, navigation | model/provider choice, prompting loop, context and session logic |
| Host services | only capabilities Zed advertises, such as filesystem/terminal/MCP | when and how to request those services |
| Configuration | Zed external-agent settings and optional MCP forwarding | native config, instructions, skills, provider credentials, native tools |
| Authentication | rendering negotiated choices and invoking the selected flow | declaring and implementing authentication methods |

Zed explicitly warns that its own profiles, rules, and skills do not automatically apply to an
external agent; the external runtime retains its native configuration and subscription model
([External Agents](https://zed.dev/docs/ai/external-agents),
[Bring Your Own Agent](https://zed.dev/blog/bring-your-own-agent-to-zed)).

This division answers the “which part is in an extension?” question: the old extension supplied
installation/launch glue. With the registry path, the manifest supplies distribution metadata,
Zed supplies the generic ACP client and UI, and the agent executable supplies behavior. A custom
adapter is still justified only when an agent does not speak ACP or needs product-specific launch
translation; it should not reimplement the client protocol.

### 1.3 Commands, authentication, capabilities, terminal, and permissions

- **Commands are session-scoped runtime data.** After session creation, the agent sends an
  `available_commands_update`; it may replace that list later. The client invokes a command by
  sending `/command` as a normal prompt. Registry metadata is not the command catalog
  ([ACP slash commands](https://agentclientprotocol.com/protocol/v1/slash-commands)).
- **Authentication is runtime-negotiated.** The initialize response exposes `authMethods`; the
  client can call `authenticate`, then create a session. Logout is capability-gated. The registry
  requires a usable Agent or Terminal authentication path rather than an environment-variable-only
  claim. Terminal Auth launches the configured executable with its declared auth arguments and
  reconnects after it exits
  ([ACP authentication](https://agentclientprotocol.com/protocol/v1/authentication),
  [registry authentication policy](https://github.com/agentclientprotocol/registry/blob/main/AUTHENTICATION.md),
  [auth-methods RFD](https://agentclientprotocol.com/rfds/auth-methods)).
- **Capabilities are negotiated, not inferred from brand.** ACP initialization carries the wire
  protocol version and both parties' capabilities; omitted features are unsupported, additive
  fields are non-breaking, and experimental extensions use namespaced `_meta`/underscore methods
  ([ACP initialization](https://agentclientprotocol.com/protocol/v1/initialization),
  [ACP extensibility](https://agentclientprotocol.com/protocol/v1/extensibility)).
- **Terminal access is an RPC contract in ACP v1, not a PTY handed to the agent.** If the client
  advertises terminal support, an agent can request terminal creation, output, wait, kill, and
  release operations. The agent may separately request user permission for a tool call, with
  once/always allow/reject choices and client policy resolution
  ([ACP terminals](https://agentclientprotocol.com/protocol/v1/terminals),
  [ACP tool calls and permissions](https://agentclientprotocol.com/protocol/v1/tool-calls)).
- **Permission UX is not containment.** ACP's threat model assumes a trusted model and agent.
  Zed states that its operating-system sandbox applies to the built-in Zed Agent, not External
  Agents or Terminal Threads
  ([ACP architecture](https://agentclientprotocol.com/get-started/architecture),
  [Zed sandboxing](https://zed.dev/docs/ai/sandboxing)).
- **Native terminal UX remains a separate lane.** Zed Terminal Threads run actual interactive
  coding CLIs in a terminal rather than translating their bytes into ACP UI events
  ([Terminal Threads](https://zed.dev/docs/ai/terminal-threads)).

### 1.4 Version status

ACP v1 is the current stable protocol. ACP v2 was published as a draft on 2026-07-20 and Zed's
inspected client still speaks v1. The draft removes client filesystem and command-execution
terminal APIs in favor of client-provided MCP tools and agent-owned, display-only terminal output.
Marion must therefore model v1 and v2 as different dialects, not assume that a larger version is a
superset of the v1 host-service API
([v2 draft announcement](https://agentclientprotocol.com/announcements/acp-v2-draft),
[v2 migration guide](https://agentclientprotocol.com/protocol/v2/migration),
[v2 filesystem/terminal RFD](https://agentclientprotocol.com/rfds/v2/client-filesystem-terminal-capabilities),
[Zed ACP client source](https://raw.githubusercontent.com/zed-industries/zed/main/crates/agent_servers/src/acp.rs)).

## 2. Gemini CLI and Antigravity: verified relationship

### 2.1 Product/status matrix

| Surface | Current role and access | Programmatic contract | Distribution/license evidence |
|---|---|---|---|
| Gemini CLI (`gemini`) | Maintained terminal agent. Consumer Google-account free/Pro/Ultra requests ended 2026-06-18; enterprise Code Assist and API-key access continue. | First-party `gemini --acp`; JSON-RPC stdio with initialize/auth/session/prompt/cancel, approval-mode and model changes, proxied filesystem calls, and client MCP servers. | Public TypeScript source, Apache-2.0; npm/npx, package-manager, and GitHub release installation documented. |
| Antigravity CLI (`agy`) | New Go terminal surface sharing the Antigravity 2.0 harness/settings, positioned as the consumer migration target. | Interactive TUI plus `agy -p --output-format stream-json` one-shot typed NDJSON. No first-party ACP mode found in the reviewed docs/changelog. | Google install scripts and compiled GitHub assets. Official repo exposes docs/examples/releases but no implementation source or displayed OSS license. |
| Antigravity managed agent | Google-hosted preview agent, separate from the local CLI. | Gemini Interactions API model `antigravity-preview-05-2026`, running in Google-hosted sandboxes; an Apache-2.0 Python SDK is published. | Managed Google service plus open-source client SDK; not evidence that the `agy` executable is open source. |

Sources for the matrix:

- Gemini CLI transition and cutoff:
  [Google Developers Blog](https://developers.googleblog.com/an-important-update-transitioning-gemini-cli-to-antigravity-cli/),
  [official Gemini CLI announcement](https://github.com/google-gemini/gemini-cli/discussions/28017).
- Gemini ACP and registry launch recipe:
  [ACP-mode documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md),
  [official ACP registry entry](https://github.com/agentclientprotocol/registry/blob/main/gemini/agent.json).
- Gemini source, license, distribution, and continued releases:
  [repository](https://github.com/google-gemini/gemini-cli),
  [license](https://github.com/google-gemini/gemini-cli/blob/main/LICENSE),
  [release documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/releases.md).
- Antigravity CLI product and distribution:
  [CLI launch](https://antigravity.google/blog/introducing-google-antigravity-cli),
  [installation](https://antigravity.google/docs/cli-install),
  [official repository](https://github.com/google-antigravity/antigravity-cli),
  [1.1.11 release](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.1.11).
- Antigravity machine-readable output: version 1.1.8 added `stream-json` events (`init`,
  `step_update`, and `result`) with tool/subagent metadata
  ([CLI changelog](https://antigravity.google/changelog?tab=cli)).
- Managed agent and SDK:
  [Managed Agents announcement](https://blog.google/innovation-and-ai/technology/developers-tools/managed-agents-gemini-api/),
  [Python SDK](https://github.com/google-antigravity/antigravity-sdk-python).

### 2.2 Corrections to likely shorthand

- **Fact:** the deprecated Gemini flag is `--experimental-acp`; Google's current flag is `--acp`.
  That flag migration is not deprecation of ACP itself
  ([Gemini CLI configuration source](https://github.com/google-gemini/gemini-cli/blob/main/packages/cli/src/config/config.ts),
  [ACP-mode documentation](https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md)).
- **Fact:** Gemini CLI's *consumer account service path* was cut over to Antigravity; the binary,
  source project, enterprise path, API-key path, and ACP mode were not announced as discontinued.
- **Fact:** Antigravity CLI's stream-JSON mode is an event-output format. The official changelog
  does not claim ACP-compatible initialization, bidirectional requests, permission replies,
  cancellation, or resumable session semantics.
- **Inference, high confidence:** Marion cannot substitute `agy` for `gemini --acp` without losing
  protocol guarantees. Absence from reviewed documentation cannot prove a hidden/private interface
  does not exist; it does establish that no supported public Antigravity CLI ACP contract was
  verifiable on the snapshot date.
- **Unknown:** Google may add a supported ACP mode later. Architecture should make that additive,
  not predeclare capabilities today.
- **Legal/product inference, not legal advice:** because §6 of the consumer terms expressly bars
  third-party service access, Marion should treat consumer `agy` and Antigravity OAuth integration
  as unavailable pending written Google authorization. Enterprise API/managed-agent use must be
  evaluated under the applicable Google Cloud agreement
  ([Antigravity terms](https://antigravity.google/terms)).

## 3. Architecture patterns for Marion

### Adopt

1. **Two lanes with a shared policy core.** Use `TransparentPty` for exact native CLI/TUI behavior
   and a versioned structured driver for ACP. Share executable resolution, provenance, lifecycle,
   secret handling, resource policy, and normalized capability decisions. Never decode a native
   PTY to manufacture ACP, and never advertise a headless ACP process as a native TUI.
2. **Three-stage agent identity.** Keep these distinct:
   - `AgentManifestV1`: stable ID, display metadata, distribution recipe, provenance, version;
   - `InstalledAgent`: resolved executable, exact package/artifact version and digest, platform;
   - `NegotiatedAgentSession`: wire dialect, runtime identity, auth methods, session capabilities,
     config options, and current slash commands.
3. **Distribution-only manifests.** Import ACP registry/local manifests for installation and launch.
   Keep vendor-specific argument injection, parsing, and measured compatibility facts in versioned
   adapter overlays. Do not put auth availability, commands, or behavioral capabilities in the
   package manifest.
4. **Explicit dynamic install/discovery.** Support registry and local/custom sources, binary,
   `npx`, and `uvx` as separate resolvers. Installation/update should be an explicit operation with
   previewed source/version/digest, an offline cache, rollback, and deterministic launch from the
   resolved record—not an implicit network action during session start.
5. **Raw plus normalized capabilities.** Preserve a version-specific wire capability object and
   map it monotonically into Marion actions. Unknown/omitted means unavailable. Marion's policy
   ceiling and the negotiated agent/client claims should meet; neither may widen the other.
6. **Version the protocol independently from packages and manifests.** An agent package version,
   manifest schema version, ACP wire dialect, and optional extension version are four different
   values. Gate v2 behind an explicit draft dialect until ACP publishes a stable replacement.
7. **Real containment.** A permission request is UI/policy, not a sandbox. Run untrusted external
   agents and their requested terminals with enforceable filesystem, process, network, environment,
   and resource boundaries. Advertise filesystem-write/terminal client capabilities only when the
   implementation actually enforces the selected scope, including symlink/path-escape rules.
8. **Agent-owned auth with secret references.** Implement negotiated Agent and Terminal Auth,
   reconnect, failure, and logout flows. Store opaque credential references in durable records,
   never tokens or auth subprocess output. Do not translate one product's OAuth credential for a
   different client.

### Reject

- A growing enum plus hard-coded table as the only discovery mechanism.
- Treating registry inclusion as execution trust, or executing an undigested archive without a
  clear user/policy decision.
- Static manifest claims for slash commands, auth methods, session capabilities, or provider
  availability.
- Parsing opaque native arguments to infer modes or silently rewriting one vendor program to a
  nominal successor.
- Claiming `stream-json`, NDJSON logs, or a PTY facade is “ACP-compatible” without the ACP request,
  response, cancellation, permission, auth, and session contracts.
- Advertising terminal/filesystem APIs optimistically and relying on permission prompts for
  containment.
- Consumer Antigravity automation or OAuth reuse without an official integration contract and
  terms clearance.

## 4. Mapping to Marion's current design and implementation

### 4.1 Preserve these decisions

- The native-facade spec's opaque `marion <facade-command> [native argv]` grammar and
  `TransparentPty | TypedAcp | RecursiveMarion` separation are directionally correct
  ([native facade design](../superpowers/specs/2026-08-09-native-harness-facade-design.md)).
- Its rule that ACP injects Marion through `session/new.mcpServers`, while a native facade preserves
  the program's own TUI, matches the protocol/client boundary. Keep it.
- [`ExecutionSurfaces`](../../crates/marion-harness/src/surfaces.rs) already separates control,
  display, and observation surfaces. Retain that model instead of collapsing “interactive” into a
  single boolean.
- [`Capabilities::meet`](../../crates/marion-harness/src/caps.rs) is the right monotone safety
  operation. Preserve the ceiling; make its inputs richer and version-aware.
- The launch-context spec's exact OS arguments, environment handling, same-UID check, and exclusion
  of transient secrets from durable journaling are compatible with a secure resolver.

### 4.2 Concrete changes to make before calling the ecosystem layer general

| Current seam | Finding | Concrete change |
|---|---|---|
| [`crates/marion-harness/src/acp.rs`](../../crates/marion-harness/src/acp.rs) | `PROTOCOL_VERSION = 1`, a bespoke initialize parser, and a static four-agent table combine protocol, discovery, and measured compatibility. | Split into `protocol/v1`, future `protocol/v2`, `manifest`, `resolver`, and `compatibility_overlay`. Generate/use official schema types where practical. Keep measured tool spellings and canned recipes in overlays keyed by agent plus tested version range. |
| ACP initialize request | Marion currently advertises filesystem read/write and terminal support unconditionally. | Build `clientCapabilities` from the effective session policy and implemented sandbox. Default dangerous host services off; fail closed if the policy cannot be enforced. |
| [`AgentHandshake`](../../crates/marion-harness/src/acp.rs) | Identity and a subset of booleans are normalized immediately. | Retain the raw versioned handshake, then derive normalized Marion capabilities. Add negotiated auth-method variants, config options, session method support, and extension metadata without treating unknown fields as true. |
| Session driver in [`acp_child.rs`](../../crates/marion-supervisor/src/acp_child.rs) | The driver covers initialize, `session/new`, prompt, cancel/kill, and selected agent requests. | Add the full supported-v1 method router incrementally, especially dynamic `available_commands_update`, Agent/Terminal Auth and logout, config updates, session load/list/resume/fork/close where negotiated, and strict request/capability validation. |
| [`Capabilities`](../../crates/marion-harness/src/caps.rs) | A fixed set of Marion booleans cannot express protocol dialect or new ACP capability objects. | Keep the normalized booleans for stable product decisions, alongside `RawProtocolCapabilities { dialect, fields, extensions }` and an explicit mapping table. Version the mapper and test unknown-field/omission behavior. |
| [`Harness` and `AgentType`](../../crates/marion-core/src/agent_type.rs) | Built-ins and the ACP OpenCode type are compiled in; external discovery is not a first-class store. | Introduce stable string `AgentPackageId`/`AgentInstallationId` values loaded from signed/pinned sources. Keep `Harness` only for native adapters that genuinely need compiled behavior; do not allocate an enum variant per registry agent. |
| [`adapter_for` / `adapter_for_type`](../../crates/marion-harness/src/adapter.rs) | Hard-coded dispatch is also doing agent selection. | Resolve package and transport first, then select a generic ACP driver plus an optional narrow compatibility overlay. Native facade descriptors continue to select compiled native adapters and preserve opaque argv. |
| `Auth::{Canned, Inherited}` in [`adapter.rs`](../../crates/marion-harness/src/adapter.rs) | This is launch-environment policy, not ACP's negotiated authentication state machine. | Keep it for native isolation semantics, but add separate `NegotiatedAuthMethod`, `AuthAttempt`, and opaque `CredentialRef` types for structured protocols. Never serialize raw secrets. |
| Gemini ACP entry | The local measurement correctly records consumer Code Assist refusing `session/new`, but that is an account-route fact, not loss of ACP. | Keep `gemini --acp`; mark eligible auth as API key, Vertex/enterprise, or another currently verified method. Report retired consumer Google-account auth distinctly. Do not redirect the `gemini` facade or ACP package to `agy`. |
| Antigravity | No current descriptor is justified by a public ACP contract, and consumer automation raises a terms conflict. | Ship no enabled `agy` adapter. If Google later authorizes it, a native `agy` command can reuse `TransparentPty`; its current one-shot stream could only enter as a separately named, capability-limited event protocol. Prefer the supported managed Interactions API/SDK for a programmatic Antigravity product, subject to its service terms. |

### 4.3 Suggested manifest/resolution boundary

```text
RegistrySource / LocalSource
          |
          v
 AgentManifestV1  -- distribution + provenance only
          |
   explicit resolve/install/verify
          v
   InstalledAgent -- immutable executable/version/digest
          |
       launch transport
      /                \
TransparentPty       ACP v1/v2 driver
native adapter       initialize + runtime negotiation
      \                /
       policy ceiling + normalized Marion capabilities
```

The native `NativeFacadeDescriptor` may reference an `InstalledAgent`, but it must not inherit ACP
runtime claims. Conversely, an ACP registry agent may use the same installed executable without
sharing the native facade's opaque argument grammar.

### 4.4 Minimum verification matrix

Before enabling dynamic agents, add tests for:

1. schema-version rejection, duplicate/reserved IDs, platform selection, semver update ordering,
   unsupported distribution types, offline cache, rollback, and deterministic resolution;
2. mandatory digest policy, archive traversal and symlink escape, executable mode, URL/source
   allowlists, corrupted cache, interrupted install, and concurrent update/launch;
3. v1 negotiation with missing/unknown capabilities, dynamic command replacement, auth success/
   failure/logout, terminal request refusal, permission cancellation, malicious path/command input,
   and complete secret redaction;
4. v1 plus gated v2-draft fixtures proving filesystem/terminal differences cannot cross dialects;
5. native facade byte/argv/exit-code transparency independent of the structured ACP suite;
6. Gemini ACP with an eligible non-consumer auth route, plus a stable diagnostic for the retired
   consumer account route; and
7. a policy test ensuring Antigravity remains undiscoverable/disabled unless both an official
   interface and an explicit legal/provider approval gate are present.

## Bottom line

The generalizable Zed pattern is **registry for distribution, protocol negotiation for behavior,
and a separate native terminal lane for exact CLI UX**. Marion's proposed facade split already has
the right shape. The highest-priority corrections are to decouple its hard-coded ACP registry from
the wire driver, stop advertising host capabilities unconditionally, add dynamic session commands
and negotiated auth, and harden installation as a security boundary. Keep Gemini CLI as Google's
verified ACP program, describe its consumer-auth cutoff precisely, and do not present Antigravity
CLI as ACP—or automate its consumer service—without a new first-party contract.
