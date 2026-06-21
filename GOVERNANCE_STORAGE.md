# Governance Storage & TTL Strategy

## Objective
Resolve the instance storage archival issue in the `multisig_governance` contract to prevent in-flight proposal data loss and guarantee finalization.

---

## 1. Storage Class Strategy
The `multisig_governance` contract stores all configuration state (admin address, target contract, version, proposal count) and in-flight proposal data (`PendingTransfer` structure under `KEY_PENDING`) in **Instance Storage**. 

In Soroban, instance storage has a specific Time-To-Live (TTL) that is checked and decremented on every ledger close. If instance storage is not accessed or explicitly bumped for a duration exceeding its TTL, it gets archived by the host. Once archived, any contract execution that tries to read or write instance storage (including admin verification via `read_admin`, loading target contract, or voting/approving) will fail.

To prevent this:
- We introduce a centralized helper function `bump_instance_ttl` that calls `env.storage().instance().extend_ttl(...)` using pre-calculated safe bounds.
- This helper is integrated across all state-modifying entrypoints (write paths) and all read-only views (read paths). By executing a TTL extension on every contract invocation, we ensure that as long as there is active interest (either querying or writing), the contract state is kept alive automatically.

---

## 2. TTL Rationale & Parameters

The Soroban SDK functions require two parameters for TTL management:
1. `threshold`: The minimum number of ledgers remaining before a bump is triggered.
2. `bump_to`: The target ledger lifetime to extend to if the threshold is breached.

We define these as:
```rust
const INSTANCE_TTL_THRESHOLD: u32 = 17280; // ~1 day (assuming 5-second ledgers)
const INSTANCE_TTL_BUMP: u32 = 518400;     // ~30 days (assuming 5-second ledgers)
```

### Rationale
- **Quorum / Timelock Lifetime**: A proposal must remain pending for a minimum timelock delay of 24 hours (`MIN_TIMELOCK_SECONDS = 86_400`) and expires after a maximum of 7 days (`PROPOSAL_TTL_SECONDS = 604_800`).
- **Safety Window**: To prevent a proposal from being archived while in-flight (waiting for the timelock to expire or gathering approvals), the instance storage TTL must comfortably exceed the maximum potential proposal lifetime (7 days + 1 day delay = 8 days).
- **Threshold (1 Day)**: Setting the threshold to 17,280 ledgers (~1 day) ensures that any interaction within a day of potential expiry triggers the extension.
- **Bump (30 Days)**: Setting the bump to 518,400 ledgers (~30 days) guarantees that the contract instance remains active for a full month on any interaction. This safely accommodates the 7-day proposal TTL + timelock and provides a significant safety margin.

---

## 3. Integration Points
The `bump_instance_ttl` helper is called in the following functions:
- `initialize`: Bumps TTL immediately upon contract creation.
- `version`: Bumps TTL on read.
- `upgrade`: Bumps TTL before upgrading contract code.
- `propose_admin_transfer`: Extends TTL on proposal creation.
- `approve_transfer`: Extends TTL on approval/voting.
- `finalize_admin_transfer`: Extends TTL before finalization.
- `cancel_admin_transfer`: Extends TTL on cancellation.
- `emergency_cancel_proposal`: Extends TTL on emergency actions.
- `expire_proposal`: Extends TTL when marking a proposal as expired.
- All view functions (`get_target`, `get_pending_transfer`, `get_pending`, `has_pending_transfer`, `get_approval_count`, `get_timelock_remaining`) and the private `read_admin` helper.

---

## 4. Verification Test
In `test.rs`, we added the integration test `test_proposal_ttl_extension_keeps_proposal_active_and_finalizable`.
This test simulates the full lifecycle of a multi-sig admin transfer proposal:
1. Propose transfer with a 24-hour timelock delay.
2. Approve from the first signer.
3. Advance the ledger sequence number by `100,000` blocks (~5.8 days) and timestamp by `200,000` seconds. This simulates a long period of inactivity during the voting window.
4. Approve from the second signer (this reads and modifies instance storage successfully, showing it was not archived).
5. Advance beyond the 24-hour timelock.
6. Finalize the proposal successfully.

This test demonstrates that the governance instance storage remains active, finalizeable, and resilient to ledger sequence advancement.
