use nomifun_common::{AttachmentId, RequirementId};
use nomifun_db::models::AttachmentRow;
use nomifun_db::{IAttachmentRepository, SqliteAttachmentRepository, init_database_memory};

fn row(attachment_id: &str, requirement_id: &str, name: &str) -> AttachmentRow {
    AttachmentRow {
        id: 0,
        attachment_id: attachment_id.to_owned(),
        requirement_id: requirement_id.to_owned(),
        file_name: name.into(),
        rel_path: format!("attachments/{requirement_id}/{attachment_id}.png"),
        mime: "image/png".into(),
        size_bytes: 123,
        created_by: Some("user".into()),
        created_at: 1,
    }
}

async fn seed_requirement(pool: &sqlx::SqlitePool, requirement_id: &str, display_no: i64) {
    sqlx::query(
        "INSERT INTO requirements \
         (requirement_id, display_no, title, tag, created_at, updated_at) \
         VALUES (?, ?, 'Req', 'default', 0, 0)",
    )
    .bind(requirement_id)
    .bind(display_no)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test]
async fn insert_list_get_delete_roundtrip() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAttachmentRepository::new(db.pool().clone());

    let requirement_1 = RequirementId::new().into_string();
    let requirement_2 = RequirementId::new().into_string();
    let requirement_3 = RequirementId::new().into_string();
    let attachment_1 = AttachmentId::new().into_string();
    let attachment_2 = AttachmentId::new().into_string();
    let attachment_3 = AttachmentId::new().into_string();
    seed_requirement(db.pool(), &requirement_1, 1).await;
    seed_requirement(db.pool(), &requirement_2, 2).await;
    seed_requirement(db.pool(), &requirement_3, 3).await;

    let inserted_1 = repo
        .insert(&row(&attachment_1, &requirement_1, "one.png"))
        .await
        .unwrap();
    let inserted_2 = repo
        .insert(&row(&attachment_2, &requirement_1, "two.png"))
        .await
        .unwrap();
    let inserted_3 = repo
        .insert(&row(&attachment_3, &requirement_2, "other.png"))
        .await
        .unwrap();
    assert!(inserted_1.id > 0);
    assert!(inserted_2.id > inserted_1.id);
    assert!(inserted_3.id > inserted_2.id);
    assert_eq!(inserted_1.attachment_id, attachment_1);

    let listed = repo.list_for_requirement(&requirement_1).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].attachment_id, attachment_1, "oldest first");
    assert_eq!(listed[1].attachment_id, attachment_2);

    let got = repo
        .get_by_attachment_id(&attachment_1)
        .await
        .unwrap()
        .expect("attachment exists");
    assert_eq!(got.id, inserted_1.id);
    assert_eq!(got.file_name, "one.png");
    assert_eq!(
        got.rel_path,
        format!("attachments/{requirement_1}/{attachment_1}.png")
    );
    assert_eq!(
        repo.get_by_id(inserted_1.id).await.unwrap().unwrap().attachment_id,
        attachment_1
    );

    assert!(repo.delete(inserted_1.id).await.unwrap());
    assert!(
        !repo.delete(inserted_1.id).await.unwrap(),
        "second delete is a no-op"
    );
    assert!(repo.get_by_id(inserted_1.id).await.unwrap().is_none());
    assert!(repo.get_by_attachment_id(&attachment_1).await.unwrap().is_none());
    assert_eq!(
        repo.list_for_requirement(&requirement_1)
            .await
            .unwrap()
            .len(),
        1
    );

    // a requirement with no attachments returns nothing
    assert!(
        repo.list_for_requirement(&requirement_3)
            .await
            .unwrap()
            .is_empty()
    );
}
