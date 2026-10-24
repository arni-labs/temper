# Decisions and tradeoffs

## D1 — Separate inline depth from background continuation hops

**Decision:** Preserve inline and reaction depth limits of 8 and add a finite 512-hop budget for each server-owned callback context lineage.

**Came up because:** The actual Foresight session completed a model/tool turn but its next background callback was refused by the shared eight-level depth counter. Main and the installed dependency are both bd15e89, so there is no newer fix to adopt.

**Options:** Increase the shared recursion limit; reset all counters at background dispatch; repeatedly resume stalled sessions; separate inline nesting from total callback hops.

**Chose separate budgets because:** Increasing the shared limit would also increase inline recursion, while resetting all counters would remove protection from background cycles. Separate counters allow finite asynchronous workflows without removing either bound. Only existing detached-task boundaries clear inline depth. No new scheduler, policy exemption, credential protocol, or app-specific logic is introduced.

**Where:** AgentContext and existing WASM/adapter/compensation spawn boundaries; docs/efforts/ARN-518/model.tla; regression and simulation tests. The guarantee is per internal context lineage, not aggregate fanout or restart-persistent resource accounting.


## Follow-up: invocation context memory


## D2 — Keep context out of initialized guest memory

**Decision:** Skip eager copies for host-context readers and reserve new pages for pointer-based modules.

**Came up because:** Foresight's actual 50-event history produced a 1,079,528-byte invocation context. The engine copied it at address 1024, overlapping static guest data beginning at 1048576 and trapping during result serialization. Both old and new application modules reproduced the failure.

**Options:** Reduce retained history; increase limits or move the fixed address; remove pointer-based support; or respect the two existing ways of reading context.

**Chose the two existing delivery paths because:** SDK modules already request their input explicitly. Pointer-based modules need a complete valid buffer, whose new pages can be bounded without assuming an unused region in existing guest memory. This preserves their entry-point signature but charges the context allocation against the memory budget; a guest with no capacity for those pages fails explicitly.

**Where:** `crates/temper-wasm/src/engine/mod.rs`; `crates/temper-wasm/tests/context_memory.rs`; ARN-518.


## D3 — Detached callbacks have no hop budget (supersedes D1's 512)

**Decision:** Remove the 512-hop budget: `MAX_CALLBACK_HOPS` and the "integration callback hop budget exhausted" refusal. Keep the count as plain data, `AgentContext.callback_depth`: one more at each integration callback, carried across detached tasks, reported on each callback's `state_change` event and as `temper.callback_depth` on dispatch and integration spans. Nothing refuses on it. A loop through detached tasks ends when the app's own data stops it.

**Came up because:** A sweep for limits with no reason outside the code (Stack, No Arbitrary Blockers). D1 picked 512 to stay finite. No resource was measured behind it, and its only sizing is one app's estimate in spec.md (a 40-turn Foresight session needs about 240 callbacks). An agent session past 512 background callbacks would be cut off between turns, exactly as the eight-step depth cut off Foresight.

**Options:** Keep 512; raise it; make it a per-app setting; carry #69's first-repeat rule across detached tasks; remove it and keep the count visible.

**Chose removal because:** A loop the caller waits for, where the server can see no end, stops at its first repeated step (temper #69, `request_context/inline_chain.rs`). A detached lap holds no request open and waits on real work, as state-timer and scheduled-action loops always have, and those were never refused: every detached boundary cloned the count (`for_background_task`), but only `for_callback` added to it or checked it, and a timer fire or scheduled action dispatches without a callback. The count was runtime-owned and never read from request headers, so no security boundary depended on it. Raising it or making it a setting would swap one unexplained number for another. #69's rule is not carried across the detach because repeating a step is what legitimate detached work looks like: a TemperPaw agent Session runs the same callbacks (ContextReady, ProviderAuthReady, ProviderResponseReady, ProcessToolCalls, HandleToolResults) on one Session every turn, and Session.CheckSandboxReady polls Provisioning to Provisioning until the sandbox is up. A runaway detached loop is then like a runaway timer: paced by its work, visible in its events and spans by its depth, and stopped by a spec guard or by the person.

**Where:** `crates/temper-server/src/request_context.rs` (`for_callback` refuses only a repeated step, `RepeatedCallback`, and counts `callback_depth`); `events.rs` (`EntityStateChange.callback_depth`); `state/dispatch/mod.rs` (`record_workflow_span_attrs`); `request_context/callback_chain_test.rs`; `tests/strict_native_callbacks/loops.rs`, where a 600-lap background WASM loop and a 600-lap compensation loop each end at their own guard with depths rising to 600. spec.md's "Total callback hops" contract and model.tla's `HopLimit` and `InlineLimit` describe D1 and are superseded (`InlineLimit` by #69); model.tla is not checked in CI.
