<!--
SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
SPDX-License-Identifier: Apache-2.0
-->

# External Validation Phase 1 gRPC Contract

External validators use Core gRPC to discover held machines, open an attempt,
allocate a targeted tenant instance, record a result, and release that instance.
NICo owns the allocation gate. It clears the gate only after external validation
passes and normal instance cleanup and local Machine Validation succeed.
Repair and other health sources remain independent.

This is the contract milestone for [design PR #6506](https://github.com/dsx-ai-factory/infra-controller/pull/6506)
and [requirement #5431](https://github.com/dsx-ai-factory/infra-controller/issues/5431).
The authoritative wire definitions are [external-validation messages](../../crates/rpc/proto/external_validation.proto)
and [Forge RPC declarations](../../crates/rpc/proto/forge.proto). The `carbide-rpc`
build generates Rust client/server types and the `ForgeApiClient` wrapper from
these sources.

## Availability

Milestone 1 publishes protobuf contracts and generated Rust interfaces. The four
new RPC handlers return `UNIMPLEMENTED`; existing method authorization may reject
calls before they reach a handler. Publishing declarations grants no additional
permissions and does not create holds or attempts.

A present `InstanceAllocationRequest.external_validation_request_id` returns
`FAILED_PRECONDITION` until request-bound allocation is implemented, including
an empty string and requests within `AllocateInstances`. Omission preserves
existing allocation behavior. The guard prevents ordinary allocation from
silently accepting a request that expects a durable validation binding.

Durable records and workflow handlers arrive in Milestone 2, request-bound
`AllocateInstance` in Milestone 3, lifecycle and recovery in Milestone 4, and
repair coexistence in Milestone 5. Enable the site policy only after the complete
Phase 1 workflow passes integration testing. DSX Exchange event enrichment
belongs to Phase 2. The following sections define the later handlers' contracts.

## Scope and authorization

Connect to the Core endpoint for the machine's site using its existing
certificate authentication and gRPC method permissions. The site is implicit
in the connected Core service. `tenant_org_id` is the configured Core tenant
organization, not a newly generated tenant for each attempt.

Each handler must verify that the authenticated caller is authorized to manage
external validation for that tenant at the connected site. Method permission
alone does not permit selecting an unrelated tenant. The hold, attempt, machine,
and allocated instance must all match the configured tenant and current cycle.
An opaque request UUID identifies an attempt; it never grants permission.
Another authorized caller for the same tenant can resume an attempt.

Allocation retains existing tenant, OS, network, resource, ownership, and
managed-state checks. The validation service cannot create, replace, or remove
the NICo-owned hold. Normal removal belongs to NICo; administrator break-glass
recovery requires a reason, audit, and resolution of any outstanding allocation
and cleanup before normal allocation resumes.

## RPC surface

All methods are on `forge.Forge`.

| RPC | Request | Response |
| --- | --- | --- |
| `ListExternalValidationHolds` | `ListExternalValidationHoldsRequest` | `ListExternalValidationHoldsResponse` |
| `StartExternalValidation` | `StartExternalValidationRequest` | `StartExternalValidationResponse` |
| `CancelExternalValidation` | `CancelExternalValidationRequest` | `ExternalValidationAttempt` |
| `CompleteExternalValidation` | `CompleteExternalValidationRequest` | `ExternalValidationAttempt` |
| `AllocateInstance` | Existing `InstanceAllocationRequest`, with optional `external_validation_request_id` field 14. | Existing `Instance`; unresolved creation returns `UNAVAILABLE`. |
| `ReleaseInstance` | Existing `InstanceReleaseRequest`, using the bound instance ID. | Existing `InstanceReleaseResult`. |

The allocation extension is additive. Request-bound batch allocation is outside
this contract. OS and network configuration stay in the existing
`InstanceAllocationRequest.config`; this workflow does not choose a validator's
image or network.

## Discovery and pagination

List on startup and periodically afterward. Required `tenant_org_id` selects
an authorized validation scope. `page_number` is optional, defaults to 1, and
accepts 1 through 21474836. `page_size` is optional, defaults to 20, and accepts
1 through 100. Explicit zero is invalid for either field.

Sort by `created_at ASC`, then `hold_id ASC`, before pagination. The response
returns `holds`, `total_count` before pagination, and the effective page number
and size. An empty authorized scope or a page beyond the end succeeds with an
empty list. Pages are live observations rather than a frozen snapshot;
periodic reconciliation remains necessary.

Active lists include Pending, AttemptOpen, AwaitingCleanup, and Recovery holds;
Satisfied holds are excluded. Repair or another health blocker does not hide a
Pending hold. Each hold includes its machine, tenant organization, stable hold
and cycle UUIDs, state, creation time, and current attempt. `attempt` is absent
in Pending. An attempt's `validation_instance_id` is absent before creation and
retained after deletion for recovery and replay. Timestamps are UTC.

## Opening an attempt

Persist a caller-generated UUID before calling Start with the exact
`machine_id`, `tenant_org_id`, and `caller_idempotency_key`. NICo scopes the key
to the connected site and configured tenant, locks the hold, and opens at most
one attempt. These response statuses are distinct from persisted attempt states:

| Status | Meaning |
| --- | --- |
| `Opened` | A new attempt or replay of the original keyed attempt, including its terminal state and original hold. |
| `AlreadyOpen` | A different key found an open attempt; use the returned identity to resume it. |
| `NoActiveHold` | No eligible hold exists; no attempt is created, and `attempt` is absent. An existing ineligible hold may still supply `hold_id`. |

Idempotent replay precedes current machine eligibility checks. Reusing a key for
another machine conflicts; a key never rebinds to a later hold or cycle. A new
attempt requires a Pending hold and a Ready, unassigned host with no
`RequestRepair` or unrelated blocking health alerts. AwaitingCleanup and
Recovery reject a new attempt until the previous allocation is resolved.

## Allocation and recovery

Persist the returned request UUID before calling `AllocateInstance`. Supply
that UUID in `external_validation_request_id`, the exact held `machine_id`,
`allow_unhealthy_machine: true`, and a config whose
`tenant.tenant_organization_id` matches the policy. The validator supplies its
normal OS and network configuration through the existing config fields.

NICo records Creating and accepted allocation inputs before dispatching
allocation, then commits the instance and its request binding together. An
identical retry returns the original `Instance`, including its generated ID
when `instance_id` was omitted. Changing accepted inputs returns
`ALREADY_EXISTS`. While allocation is Creating, return `UNAVAILABLE`; the caller
retries the same request and inputs or reads the hold list for allocation state
and the saved instance ID. It must not generate a new attempt to recover an
ambiguous outcome.

The request permits bypass of this workflow's hold only. NICo rechecks unrelated
blocking health alerts, repair admission, ownership, and provisionability at
allocation commit. Labels cannot establish a binding. A closed or superseded
request cannot allocate another instance, even after its original instance was
released.

## Cancellation and completion

Cancel requires `request_id`, `tenant_org_id`, and a nonempty `details` reason.
Only an Open attempt with allocation state NotStarted can be cancelled. Record
Cancelled, close the attempt, and keep the hold Pending. Identical cancellation
replay returns the original Cancelled attempt, including after a newer attempt
opens, without modifying the newer attempt or its hold. Replay lookup precedes
current-attempt checks. Changed details or a stale request without a matching
recorded cancellation conflicts.
Creating and Created must be reconciled or completed and released instead.

Complete requires `request_id`, `tenant_org_id`, the bound Core
`validation_instance_id`, and outcome Passed, Failed, or Cancelled. Unspecified
or unknown outcomes are invalid. The request must match the active attempt,
current cycle, configured tenant, and saved instance binding in allocation
state Created. Identical terminal replay succeeds even after instance deletion;
a changed instance, result, or details conflicts.

Completion `details` is optional: omission is distinct from an explicitly empty
string and replay must preserve that presence. Both completion and cancellation
accept at most 4096 UTF-8 bytes of details; cancellation requires at least one
byte. These limits apply to multibyte text as well as ASCII.

For every terminal outcome, the validator must call `ReleaseInstance` using the
bound instance ID. A recorded result moves the hold to AwaitingCleanup. Passed
satisfies the hold only after successful cleanup and local validation, and only
if repair has not invalidated that attempt. Failed or Cancelled returns to
Pending after successful cleanup. Missing results, unexpected deletion,
ownership mismatch, or failed cleanup/local validation never satisfy the hold.
Deletion alone is not success.

Holds and attempts have no external-validation expiry. The validator owns its
execution deadline, including work lasting days, reports a result, and releases
its instance. Existing Core allocation and cleanup recovery still apply. A
missing service or result never releases capacity.

## Status codes

| gRPC code | Meaning and recovery |
| --- | --- |
| `INVALID_ARGUMENT` | Required input is missing, a UUID is malformed, pagination/details bounds are exceeded, or an outcome is unspecified or unknown. Correct the request. |
| `UNAUTHENTICATED` | Authentication failed. Use valid credentials. |
| `PERMISSION_DENIED` | Caller lacks method or tenant/site authorization. Request identity is not sufficient. |
| `NOT_FOUND` | Machine or request does not exist in the authorized scope. Verify identity; absence of a hold instead returns `NoActiveHold`. |
| `ALREADY_EXISTS` | Idempotency key targets another machine, allocation inputs changed, or a terminal result conflicts. Preserve the original operation identity and resolve the conflict. |
| `FAILED_PRECONDITION` | Allocation, cleanup, recovery, or attempt state blocks the requested mutation. Reconcile the existing work first. Milestone 1 also uses this code for unsupported request-bound allocation. |
| `UNAVAILABLE` | Site operation is transiently unavailable or request-bound allocation is still Creating. Replay only the same mutation identity and inputs. |
| `DEADLINE_EXCEEDED` | The call timed out; a mutation may have committed. Reconcile or replay the same identity. |
| `UNIMPLEMENTED` | The workflow handlers are contract declarations in Milestone 1. |

Do not infer that a timed-out allocation failed or that an empty inventory proves
no instance was committed. Resolve allocation and cleanup, then fence or close
the old attempt before opening a new one. Repair requested during an attempt
prevents that attempt from satisfying the hold; fresh validation follows
successful repair.
