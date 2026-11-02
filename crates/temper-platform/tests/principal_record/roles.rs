//! A user's roles in an app are one record's list, its `principal_record_set`:
//! Cedar reads them as a set on the principal, fresh on every call, and a
//! revoke takes out a role by its name.

use axum::http::StatusCode;

use super::fixture::{
    ALICE, BOB, add_role_record, agent_for, app, cook_note, in_person, move_role, roles,
};

/// Alice holds `Cook` and `Shopper`.
async fn cook_and_shopper(seed: u64) -> temper_platform::state::PlatformState {
    let state = app(seed).await;
    add_role_record(&state, ALICE).await;
    assert!(move_role(&state, ALICE, "Grant", "Cook").await);
    assert!(move_role(&state, ALICE, "Grant", "Shopper").await);
    assert_eq!(roles(&state, ALICE).await, ["Cook", "Shopper"]);
    state
}

#[tokio::test]
async fn a_role_in_the_set_admits_and_its_revoke_refuses_the_next_call() {
    let state = cook_and_shopper(21).await;
    let alice = in_person(ALICE);
    assert_eq!(cook_note(&state, &alice).await, StatusCode::OK);
    assert_eq!(
        cook_note(&state, &in_person(BOB)).await,
        StatusCode::FORBIDDEN,
        "a user with no Role record"
    );
    assert!(move_role(&state, ALICE, "Revoke", "Cook").await);
    assert_eq!(roles(&state, ALICE).await, ["Shopper"]);
    assert_eq!(
        cook_note(&state, &alice).await,
        StatusCode::FORBIDDEN,
        "the same token, the call after the revoke answered"
    );
    assert!(move_role(&state, ALICE, "Grant", "Cook").await);
    assert_eq!(cook_note(&state, &alice).await, StatusCode::OK);
}

#[tokio::test]
async fn an_agent_holds_the_roles_of_the_user_it_acts_for() {
    let state = cook_and_shopper(22).await;
    let agent = agent_for(ALICE);
    assert_eq!(cook_note(&state, &agent).await, StatusCode::OK);
    assert!(move_role(&state, ALICE, "Revoke", "Cook").await);
    assert_eq!(
        cook_note(&state, &agent).await,
        StatusCode::FORBIDDEN,
        "acting for a user who no longer holds the role"
    );
    assert_eq!(
        cook_note(&state, &agent_for(BOB)).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn revoking_a_role_not_held_and_granting_one_held_change_nothing() {
    let state = cook_and_shopper(23).await;
    assert!(
        move_role(&state, ALICE, "Revoke", "Baker").await,
        "a revoke of a role not held is taken, and removes nothing"
    );
    assert_eq!(roles(&state, ALICE).await, ["Cook", "Shopper"]);
    assert!(
        !move_role(&state, ALICE, "Grant", "Cook").await,
        "a grant of a role held is refused by its guard"
    );
    assert_eq!(roles(&state, ALICE).await, ["Cook", "Shopper"]);
}

#[tokio::test]
async fn two_revokes_of_different_roles_both_land() {
    let state = cook_and_shopper(24).await;
    assert!(move_role(&state, ALICE, "Grant", "Baker").await);
    let (cook, shopper) = tokio::join!(
        move_role(&state, ALICE, "Revoke", "Cook"),
        move_role(&state, ALICE, "Revoke", "Shopper"),
    );
    assert!(cook && shopper);
    assert_eq!(roles(&state, ALICE).await, ["Baker"]);
    assert_eq!(
        cook_note(&state, &in_person(ALICE)).await,
        StatusCode::FORBIDDEN
    );
}
