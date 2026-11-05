use http::{Method, StatusCode};

use forklift::meta::{ManagedGrant, ManagedRBAC, ManagedRole, Permission};

use forklift::testing::api::new_test_server;

#[tokio::test]
async fn managed_rbac_read_only_via_api() {
    let srv = new_test_server().await;

    let desired = ManagedRBAC {
        roles: vec![ManagedRole {
            name: "readonly".to_string(),
            permissions: vec![Permission {
                repo_pattern: "*".to_string(),
                actions: "read".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }],
        group_roles: vec![ManagedGrant {
            subject: "/devs".to_string(),
            role: "readonly".to_string(),
        }],
        user_roles: vec![ManagedGrant {
            subject: "alice".to_string(),
            role: "readonly".to_string(),
        }],
        ..Default::default()
    };
    srv.store
        .apply_managed_rbac(desired)
        .await
        .expect("seed managed rbac");

    // GET /roles exposes the managed flag and the permission id.
    let roles = srv.admin_do(Method::GET, "/roles", "").await.json();
    let readonly = roles
        .as_array()
        .expect("roles")
        .iter()
        .find(|r| r["name"] == "readonly")
        .unwrap_or_else(|| panic!("readonly role not reported: {roles}"))
        .clone();
    let role_id = readonly["id"].as_i64().expect("role id");
    assert_ne!(role_id, 0, "{roles}");
    assert_eq!(readonly["managed"], true, "{roles}");
    let perms = readonly["permissions"].as_array().expect("permissions");
    assert_eq!(perms.len(), 1, "{readonly}");
    let perm_id = perms[0]["id"].as_i64().expect("permission id");

    // Deleting a managed role is rejected.
    let resp = srv
        .admin_do(Method::DELETE, &format!("/roles/{role_id}"), "")
        .await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "delete managed role");

    // Adding a permission to a managed role is rejected.
    let resp = srv
        .admin_do(
            Method::POST,
            &format!("/roles/{role_id}/permissions"),
            r#"{"repo_pattern":"*","actions":["write"]}"#,
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::CONFLICT,
        "add perm to managed role"
    );

    // Deleting a managed permission is rejected.
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!("/roles/{role_id}/permissions/{perm_id}"),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "delete managed perm");

    // Deleting a managed group mapping is rejected.
    let mappings = srv
        .store
        .list_group_mappings()
        .await
        .expect("list group mappings");
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!("/group-mappings/{}", mappings[0].id),
            "",
        )
        .await;
    assert_eq!(resp.status, StatusCode::CONFLICT, "delete managed mapping");

    // Removing a managed user-role assignment is rejected.
    let alice = srv
        .store
        .get_user_by_username("alice")
        .await
        .expect("alice");
    let resp = srv
        .admin_do(
            Method::DELETE,
            &format!("/users/{}/roles/{role_id}", alice.id),
            "",
        )
        .await;
    assert_eq!(
        resp.status,
        StatusCode::CONFLICT,
        "remove managed user role"
    );
}

#[tokio::test]
async fn unmanaged_role_still_mutable() {
    let srv = new_test_server().await;

    // A role created via the API is unmanaged and fully mutable.
    let resp = srv
        .admin_do(
            Method::POST,
            "/roles",
            r#"{"name":"manual","description":"x","permissions":[{"repo_pattern":"*","actions":["read"]}]}"#,
        )
        .await;
    assert_eq!(resp.status, StatusCode::CREATED, "create role");
    let role = resp.json();
    assert_eq!(
        role["managed"], false,
        "an API-created role must be unmanaged"
    );
    let id = role["id"].as_i64().expect("role id");
    let resp = srv
        .admin_do(Method::DELETE, &format!("/roles/{id}"), "")
        .await;
    assert_eq!(resp.status, StatusCode::NO_CONTENT, "delete unmanaged role");
}
