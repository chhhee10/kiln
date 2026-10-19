use kiln_auth::*;
use kiln_store::{Store, setting_keys};
use kiln_types::api::{
    CreateUserRequest, LoginRequest, PairCompleteRequest, RegisterRequest, SetupRequest,
    UpdateUserRequest,
};
use kiln_types::{ClientKind, User, UserRole, UserStatus};

const PW: &str = "correct horse battery";

async fn auth() -> Auth {
    Auth::with_cost(
        Store::open_in_memory().await.unwrap(),
        PasswordCost::INSECURE_FAST,
    )
}

async fn setup(a: &Auth) -> (User, String) {
    a.setup(SetupRequest {
        admin_email: "admin@example.com".into(),
        admin_name: "Ada Admin".into(),
        password: PW.into(),
        allowed_domain: "Example.com".into(),
    })
    .await
    .unwrap()
}

fn login(email: &str, pw: &str) -> LoginRequest {
    LoginRequest {
        email: email.into(),
        password: pw.into(),
        client: None,
        device_name: None,
    }
}

#[tokio::test]
async fn setup_only_once_and_first_user_is_admin() {
    let a = auth().await;
    assert!(a.setup_required().await.unwrap());
    // Registration is closed before setup.
    let e = a
        .register(RegisterRequest {
            email: "x@example.com".into(),
            name: "X".into(),
            password: PW.into(),
        })
        .await;
    assert!(matches!(e, Err(AuthError::SetupRequired)));

    let (admin, token) = setup(&a).await;
    assert_eq!(admin.role, UserRole::Admin);
    assert!(!a.setup_required().await.unwrap());
    assert_eq!(
        a.allowed_domain().await.unwrap().as_deref(),
        Some("example.com")
    );
    assert_eq!(
        a.store()
            .get_setting::<bool>(setting_keys::SETUP_DONE)
            .await
            .unwrap(),
        Some(true)
    );
    assert_eq!(a.authenticate(&token).await.unwrap().id, admin.id);

    let again = a
        .setup(SetupRequest {
            admin_email: "evil@example.com".into(),
            admin_name: "Evil".into(),
            password: PW.into(),
            allowed_domain: "evil.com".into(),
        })
        .await;
    assert!(matches!(again, Err(AuthError::AlreadySetUp)));
    assert_eq!(
        a.allowed_domain().await.unwrap().as_deref(),
        Some("example.com")
    );
}

#[tokio::test]
async fn concurrent_setup_creates_one_admin() {
    let a = std::sync::Arc::new(auth().await);
    let mut hs = Vec::new();
    for i in 0..5 {
        let a = a.clone();
        hs.push(tokio::spawn(async move {
            a.setup(SetupRequest {
                admin_email: format!("admin{i}@example.com"),
                admin_name: "A".into(),
                password: PW.into(),
                allowed_domain: "example.com".into(),
            })
            .await
            .is_ok()
        }));
    }
    let mut ok = 0;
    for h in hs {
        ok += h.await.unwrap() as u32;
    }
    assert_eq!(ok, 1);
    assert_eq!(a.store().count_users().await.unwrap(), 1);
}

#[tokio::test]
async fn setup_validates_input() {
    let a = auth().await;
    let weak = a
        .setup(SetupRequest {
            admin_email: "admin@example.com".into(),
            admin_name: "A".into(),
            password: "short".into(),
            allowed_domain: "example.com".into(),
        })
        .await;
    assert!(matches!(weak, Err(AuthError::Invalid(_))));
    let bad_domain = a
        .setup(SetupRequest {
            admin_email: "admin@example.com".into(),
            admin_name: "A".into(),
            password: PW.into(),
            allowed_domain: "nope".into(),
        })
        .await;
    assert!(matches!(bad_domain, Err(AuthError::Invalid(_))));
    assert!(a.setup_required().await.unwrap());
}

#[tokio::test]
async fn login_errors_are_generic() {
    let a = auth().await;
    setup(&a).await;
    let unknown = a.login(login("nobody@example.com", PW)).await.unwrap_err();
    let wrong = a
        .login(login("admin@example.com", "wrong password!!"))
        .await
        .unwrap_err();
    assert!(matches!(unknown, AuthError::InvalidCredentials));
    assert!(matches!(wrong, AuthError::InvalidCredentials));
    assert_eq!(unknown.to_string(), wrong.to_string());
    assert_eq!(unknown.to_api_error(), wrong.to_api_error());

    let (u, token) = a.login(login("ADMIN@example.com", PW)).await.unwrap();
    assert_eq!(u.email, "admin@example.com");
    assert_eq!(a.authenticate(&token).await.unwrap().id, u.id);
}

#[tokio::test]
async fn login_registers_device() {
    let a = auth().await;
    let (admin, _) = setup(&a).await;
    let (_, _t) = a
        .login(LoginRequest {
            email: "admin@example.com".into(),
            password: PW.into(),
            client: Some(ClientKind::Desktop),
            device_name: Some("mac-mini".into()),
        })
        .await
        .unwrap();
    let devices = a.store().list_devices(admin.id).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].kind, ClientKind::Desktop);
}

#[tokio::test]
async fn register_enforces_allowlist() {
    let a = auth().await;
    setup(&a).await;
    let e = a
        .register(RegisterRequest {
            email: "x@evil.com".into(),
            name: "X".into(),
            password: PW.into(),
        })
        .await;
    assert!(matches!(e, Err(AuthError::NotAllowlisted)));
    let e = a
        .register(RegisterRequest {
            email: "x@sub.example.com".into(),
            name: "X".into(),
            password: PW.into(),
        })
        .await;
    assert!(matches!(e, Err(AuthError::NotAllowlisted)));
    let e = a
        .register(RegisterRequest {
            email: "x@example.com".into(),
            name: "X".into(),
            password: "short".into(),
        })
        .await;
    assert!(matches!(e, Err(AuthError::Invalid(_))));

    let (u, token) = a
        .register(RegisterRequest {
            email: "Jin@Example.com".into(),
            name: "Jin Park".into(),
            password: PW.into(),
        })
        .await
        .unwrap();
    assert_eq!(u.role, UserRole::Member);
    assert_eq!(a.authenticate(&token).await.unwrap().id, u.id);
    let dup = a
        .register(RegisterRequest {
            email: "jin@example.com".into(),
            name: "J".into(),
            password: PW.into(),
        })
        .await;
    assert!(matches!(dup, Err(AuthError::EmailTaken)));
}

#[tokio::test]
async fn authenticate_and_logout() {
    let a = auth().await;
    let (_, token) = setup(&a).await;
    assert!(matches!(
        a.authenticate("garbage").await,
        Err(AuthError::Unauthorized)
    ));
    assert!(matches!(
        a.authenticate("").await,
        Err(AuthError::Unauthorized)
    ));
    a.logout(&token).await.unwrap();
    assert!(matches!(
        a.authenticate(&token).await,
        Err(AuthError::Unauthorized)
    ));
    // Logging out twice is fine.
    a.logout(&token).await.unwrap();
}

#[tokio::test]
async fn disabled_users_cannot_login_or_authenticate() {
    let a = auth().await;
    let (admin, _) = setup(&a).await;
    let member = a
        .create_user(
            &admin,
            CreateUserRequest {
                email: "m@anywhere.io".into(),
                name: "Mem".into(),
                password: PW.into(),
                role: UserRole::Member,
            },
        )
        .await
        .unwrap();
    let (_, token) = a.login(login("m@anywhere.io", PW)).await.unwrap();
    a.update_user(
        &admin,
        member.id,
        UpdateUserRequest {
            status: Some(UserStatus::Disabled),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        a.authenticate(&token).await,
        Err(AuthError::Unauthorized)
    ));
    assert!(matches!(
        a.login(login("m@anywhere.io", PW)).await,
        Err(AuthError::Disabled)
    ));
    // Wrong password on a disabled account still says only "invalid".
    assert!(matches!(
        a.login(login("m@anywhere.io", "wrong password!")).await,
        Err(AuthError::InvalidCredentials)
    ));

    // Re-enabled → can log in again.
    a.update_user(
        &admin,
        member.id,
        UpdateUserRequest {
            status: Some(UserStatus::Active),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    a.login(login("m@anywhere.io", PW)).await.unwrap();
}

#[tokio::test]
async fn admin_only_operations() {
    let a = auth().await;
    let (admin, _) = setup(&a).await;
    let (member, _) = a
        .register(RegisterRequest {
            email: "m@example.com".into(),
            name: "Mem".into(),
            password: PW.into(),
        })
        .await
        .unwrap();
    let req = CreateUserRequest {
        email: "z@x.io".into(),
        name: "Z".into(),
        password: PW.into(),
        role: UserRole::Member,
    };
    assert!(matches!(
        a.create_user(&member, req.clone()).await,
        Err(AuthError::Forbidden(_))
    ));
    assert!(matches!(
        a.set_allowed_domain(&member, "evil.com").await,
        Err(AuthError::Forbidden(_))
    ));
    assert!(matches!(
        a.update_user(
            &member,
            admin.id,
            UpdateUserRequest {
                status: Some(UserStatus::Disabled),
                ..Default::default()
            }
        )
        .await,
        Err(AuthError::Forbidden(_))
    ));
    let z = a.create_user(&admin, req).await.unwrap();
    assert_eq!(z.email, "z@x.io");
    assert_eq!(
        a.set_allowed_domain(&admin, "@New.org").await.unwrap(),
        "new.org"
    );

    // Last admin can't demote or disable themselves.
    let e = a
        .update_user(
            &admin,
            admin.id,
            UpdateUserRequest {
                role: Some(UserRole::Member),
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(e, Err(AuthError::Forbidden(_))));
    // With a second admin it's allowed.
    let m2 = a
        .update_user(
            &admin,
            member.id,
            UpdateUserRequest {
                role: Some(UserRole::Admin),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(m2.is_admin());
    a.update_user(
        &admin,
        admin.id,
        UpdateUserRequest {
            role: Some(UserRole::Member),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn password_changes_revoke_sessions() {
    let a = auth().await;
    let (admin, t1) = setup(&a).await;
    let (_, t2) = a.login(login("admin@example.com", PW)).await.unwrap();
    assert!(matches!(
        a.change_password(&admin, "wrong password!", "another long pw")
            .await,
        Err(AuthError::InvalidCredentials)
    ));
    let t3 = a
        .change_password(&admin, PW, "another long pw")
        .await
        .unwrap();
    assert!(a.authenticate(&t1).await.is_err());
    assert!(a.authenticate(&t2).await.is_err());
    assert!(a.authenticate(&t3).await.is_ok());
    assert!(a.login(login("admin@example.com", PW)).await.is_err());
    a.login(login("admin@example.com", "another long pw"))
        .await
        .unwrap();

    // Admin reset of another user's password.
    let m = a
        .create_user(
            &admin,
            CreateUserRequest {
                email: "m@x.io".into(),
                name: "M".into(),
                password: PW.into(),
                role: UserRole::Member,
            },
        )
        .await
        .unwrap();
    let (_, mt) = a.login(login("m@x.io", PW)).await.unwrap();
    a.update_user(
        &admin,
        m.id,
        UpdateUserRequest {
            password: Some("reset password 1".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(a.authenticate(&mt).await.is_err());
    a.login(login("m@x.io", "reset password 1")).await.unwrap();
}

#[tokio::test]
async fn pairing_flow() {
    let a = auth().await;
    let (admin, _) = setup(&a).await;
    let (code, expires) = a.start_pairing(&admin).await.unwrap();
    assert_eq!(code.len(), 8);
    let mins = (expires - chrono::Utc::now()).num_minutes();
    assert!((9..=10).contains(&mins), "{mins}");

    // Code is stored hashed, never in plain text.
    let n: i64 = sqlx_count(a.store(), &code).await;
    assert_eq!(n, 0);

    let typed = format!("{}-{}", code[..4].to_lowercase(), &code[4..]);
    let (u, token) = a
        .complete_pairing(PairCompleteRequest {
            code: typed,
            client: ClientKind::Android,
            device_name: "Pixel".into(),
        })
        .await
        .unwrap();
    assert_eq!(u.id, admin.id);
    assert_eq!(a.authenticate(&token).await.unwrap().id, admin.id);
    let devices = a.store().list_devices(admin.id).await.unwrap();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].kind, ClientKind::Android);

    // Single use.
    let again = a
        .complete_pairing(PairCompleteRequest {
            code: code.clone(),
            client: ClientKind::Android,
            device_name: "x".into(),
        })
        .await;
    assert!(matches!(again, Err(AuthError::InvalidPairingCode)));
    let bogus = a
        .complete_pairing(PairCompleteRequest {
            code: "ZZZZZZZZ".into(),
            client: ClientKind::Tui,
            device_name: "x".into(),
        })
        .await;
    assert!(matches!(bogus, Err(AuthError::InvalidPairingCode)));
    let short = a
        .complete_pairing(PairCompleteRequest {
            code: "abc".into(),
            client: ClientKind::Tui,
            device_name: "x".into(),
        })
        .await;
    assert!(matches!(short, Err(AuthError::InvalidPairingCode)));
}

async fn sqlx_count(store: &Store, code: &str) -> i64 {
    kiln_store::sqlx::query_scalar("SELECT COUNT(*) FROM pairing_codes WHERE code_hash = ?")
        .bind(code)
        .fetch_one(store.pool())
        .await
        .unwrap()
}
