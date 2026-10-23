//! Group memberships as Cedar parents (ADR-0183).
//!
//! A membership is one `GroupMembership` record: a `group` (a Cedar entity uid,
//! `Type::"id"`) and a `member` (a principal id), `Active` or `Ended`. The
//! engine keeps the tenant's memberships here, keyed by member and then group,
//! and makes a principal a child of the groups a policy can ask about
//! (`principal in ...`) when it is an active member of them. A principal is
//! never given every group it is in: a check looks up only the candidates the
//! tenant's policies name, so its cost follows the policy text, not how many
//! groups a person has joined.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::RwLock;

use cedar_policy::EntityUid;
use sha2::{Digest, Sha256};

/// The kernel entity type that holds one membership.
pub const GROUP_MEMBERSHIP_TYPE: &str = "GroupMembership";

/// The status of a membership that makes its member part of its group.
pub const ACTIVE_STATUS: &str = "Active";

/// The id of the one record for `member` in `group`: hex sha256 of the group's
/// uid (as Cedar writes it) and the member, joined by a newline. A record whose
/// id is not this for its own fields is not a membership.
pub fn group_membership_id(group: &str, member: &str) -> Option<String> {
    let group = normalized_group(group)?;
    let mut hasher = Sha256::new();
    hasher.update(group.as_bytes());
    hasher.update(b"\n");
    hasher.update(member.as_bytes());
    Some(format!("{:x}", hasher.finalize()))
}

/// The group's uid as Cedar prints it, so `T::"a"` written any valid way is one key.
fn normalized_group(group: &str) -> Option<String> {
    EntityUid::from_str(group).ok().map(|uid| uid.to_string())
}

/// One membership as its record holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MembershipRecord<'a> {
    pub tenant: &'a str,
    pub record_id: &'a str,
    pub group: &'a str,
    pub member: &'a str,
    pub active: bool,
    /// The record's event sequence number: a change older than the one already
    /// applied is ignored, so a slow read during a fill never undoes a newer
    /// transition.
    pub sequence_nr: u64,
}

#[derive(Debug, Default)]
struct TenantMemberships {
    /// The tenant's records were all read once, so an absent key is an answer.
    filled: bool,
    /// member -> group -> active.
    by_member: BTreeMap<String, BTreeMap<String, bool>>,
    /// record id -> (group, member, sequence_nr) last applied for it.
    by_record: BTreeMap<String, (String, String, u64)>,
}

/// Every tenant's memberships. Memory is one entry per membership record, as
/// the entity index holds one per entity.
#[derive(Debug, Default)]
pub struct GroupMemberships {
    tenants: RwLock<BTreeMap<String, TenantMemberships>>,
}

/// Why a record was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipSkip {
    /// The record's id is not `group_membership_id(group, member)`.
    IdMismatch,
    /// A newer change of this record was applied already.
    Stale,
}

impl GroupMemberships {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one record's state. A record whose fields no longer match its id
    /// takes back whatever it granted before.
    pub fn apply(&self, record: &MembershipRecord<'_>) -> Result<(), MembershipSkip> {
        let mut tenants = self.tenants.write().unwrap_or_else(|e| e.into_inner()); // ci-ok: poison keeps the data
        let tenant = tenants.entry(record.tenant.to_string()).or_default();
        if let Some((_, _, applied)) = tenant.by_record.get(record.record_id)
            && *applied > record.sequence_nr
        {
            return Err(MembershipSkip::Stale);
        }
        // Whatever this record granted before is withdrawn first.
        if let Some((group, member, _)) = tenant.by_record.remove(record.record_id) {
            tenant.set(&member, &group, false);
        }
        let valid = normalized_group(record.group).filter(|group| {
            group_membership_id(group, record.member).as_deref() == Some(record.record_id)
        });
        let Some(group) = valid else {
            return Err(MembershipSkip::IdMismatch);
        };
        tenant.set(record.member, &group, record.active);
        tenant.by_record.insert(
            record.record_id.to_string(),
            (group, record.member.to_string(), record.sequence_nr),
        );
        Ok(())
    }

    /// The record is gone (deleted): take back what it granted. Its sequence
    /// number is kept, so a slower read of its earlier state is still stale.
    pub fn withdraw(&self, tenant: &str, record_id: &str, sequence_nr: u64) {
        let mut tenants = self.tenants.write().unwrap_or_else(|e| e.into_inner()); // ci-ok: poison keeps the data
        let Some(tenant) = tenants.get_mut(tenant) else {
            return;
        };
        if let Some((group, member, applied)) = tenant.by_record.get_mut(record_id) {
            let (group, member) = (group.clone(), member.clone());
            *applied = (*applied).max(sequence_nr);
            tenant.set(&member, &group, false);
        }
    }

    /// Mark the tenant's records as all read: from now on an absent membership is
    /// a "no". Before this, every lookup in the tenant answers no (fail closed).
    pub fn mark_filled(&self, tenant: &str) {
        let mut tenants = self.tenants.write().unwrap_or_else(|e| e.into_inner()); // ci-ok: poison keeps the data
        tenants.entry(tenant.to_string()).or_default().filled = true;
    }

    pub fn is_filled(&self, tenant: &str) -> bool {
        let tenants = self.tenants.read().unwrap_or_else(|e| e.into_inner()); // ci-ok: poison keeps the data
        tenants.get(tenant).is_some_and(|t| t.filled)
    }

    /// Whether `member` is an active member of `group` (as [`EntityUid`] prints it).
    /// A tenant not yet filled answers false.
    pub fn contains(&self, tenant: &str, group: &str, member: &str) -> bool {
        let tenants = self.tenants.read().unwrap_or_else(|e| e.into_inner()); // ci-ok: poison keeps the data
        tenants.get(tenant).is_some_and(|t| {
            t.filled
                && t.by_member
                    .get(member)
                    .and_then(|groups| groups.get(group))
                    .copied()
                    .unwrap_or(false)
        })
    }

    /// Active memberships held for a tenant (for metrics and measurements).
    pub fn active_count(&self, tenant: &str) -> usize {
        let tenants = self.tenants.read().unwrap_or_else(|e| e.into_inner()); // ci-ok: poison keeps the data
        tenants.get(tenant).map_or(0, |t| {
            t.by_member
                .values()
                .map(|groups| groups.values().filter(|a| **a).count())
                .sum()
        })
    }

    /// Forget a tenant (its policies and records were removed).
    pub fn remove_tenant(&self, tenant: &str) {
        let mut tenants = self.tenants.write().unwrap_or_else(|e| e.into_inner()); // ci-ok: poison keeps the data
        tenants.remove(tenant);
    }
}

impl TenantMemberships {
    fn set(&mut self, member: &str, group: &str, active: bool) {
        if active {
            self.by_member
                .entry(member.to_string())
                .or_default()
                .insert(group.to_string(), true);
        } else if let Some(groups) = self.by_member.get_mut(member) {
            groups.remove(group);
            if groups.is_empty() {
                self.by_member.remove(member);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: &str = "Team::\"t1\"";

    fn record<'a>(id: &'a str, member: &'a str, active: bool, seq: u64) -> MembershipRecord<'a> {
        MembershipRecord {
            tenant: "t",
            record_id: id,
            group: TEAM,
            member,
            active,
            sequence_nr: seq,
        }
    }

    #[test]
    fn a_membership_counts_only_once_the_tenant_is_filled_and_while_it_is_active() {
        let set = GroupMemberships::new();
        let id = group_membership_id(TEAM, "alice").unwrap();
        set.apply(&record(&id, "alice", true, 1)).unwrap();
        assert!(!set.contains("t", TEAM, "alice"), "not filled: fail closed");
        set.mark_filled("t");
        assert!(set.contains("t", TEAM, "alice"));
        assert!(!set.contains("t", TEAM, "bob"));
        set.apply(&record(&id, "alice", false, 2)).unwrap();
        assert!(!set.contains("t", TEAM, "alice"), "ended");
        assert_eq!(
            set.apply(&record(&id, "alice", true, 1)),
            Err(MembershipSkip::Stale),
            "an older read never undoes a newer change"
        );
        assert!(!set.contains("t", TEAM, "alice"));
    }

    #[test]
    fn a_record_whose_id_is_not_its_fields_grants_nothing_and_takes_back_what_it_granted() {
        let set = GroupMemberships::new();
        set.mark_filled("t");
        let id = group_membership_id(TEAM, "alice").unwrap();
        assert_eq!(
            set.apply(&record(&id, "mallory", true, 1)),
            Err(MembershipSkip::IdMismatch)
        );
        assert!(!set.contains("t", TEAM, "mallory"));
        set.apply(&record(&id, "alice", true, 2)).unwrap();
        assert_eq!(
            set.apply(&record(&id, "mallory", true, 3)),
            Err(MembershipSkip::IdMismatch)
        );
        assert!(
            !set.contains("t", TEAM, "alice"),
            "the record no longer says alice"
        );
        assert!(!set.contains("t", TEAM, "mallory"));
    }

    #[test]
    fn a_deleted_record_grants_nothing_and_an_older_read_does_not_bring_it_back() {
        let set = GroupMemberships::new();
        set.mark_filled("t");
        let id = group_membership_id(TEAM, "alice").unwrap();
        set.apply(&record(&id, "alice", true, 3)).unwrap();
        set.withdraw("t", &id, 4);
        assert!(!set.contains("t", TEAM, "alice"));
        assert_eq!(
            set.apply(&record(&id, "alice", true, 3)),
            Err(MembershipSkip::Stale)
        );
        assert!(!set.contains("t", TEAM, "alice"));
    }

    #[test]
    fn a_group_is_one_key_however_its_uid_is_written() {
        assert_eq!(
            group_membership_id("Team::\"t1\"", "a"),
            group_membership_id("Team :: \"t1\"", "a")
        );
        assert_eq!(group_membership_id("not a uid", "a"), None);
    }
}
