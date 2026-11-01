//! An upgrade is compared with the installed version's own files: an entity
//! set another app owns is refused when the upgrade newly declares it, kept
//! as a warning when the installed version already did, and kept as a
//! warning that says so when the installed version cannot be read.

use super::rollback_tests::{cache, record};
use super::*;

const LEDGER: &str = "owner-ledger";
const NOTES: &str = "owner-notes";

/// A CSDL with the entity types `types`, each in a set named `<type>s`.
fn csdl(types: &[&str]) -> String {
    let entity_types: String = types
        .iter()
        .map(|name| {
            format!(
                r#"<EntityType Name="{name}"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>"#
            )
        })
        .collect();
    let sets: String = types
        .iter()
        .map(|name| format!(r#"<EntitySet Name="{name}s" EntityType="Test.Upgrade.{name}"/>"#))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Test.Upgrade" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      {entity_types}
      <EntityContainer Name="Upgrade">{sets}</EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
"#
    )
}

/// `app` in `root`, declaring the entity types `types`, listed as an install writes it.
fn write_declaring(root: &Path, app: &str, types: &[&str]) {
    let app_dir = root.join(app);
    std::fs::create_dir_all(app_dir.join("specs")).expect("specs dir");
    std::fs::write(app_dir.join("app.toml"), format!("name = \"{app}\"\n")).expect("app.toml");
    std::fs::write(app_dir.join("APP.md"), format!("# {app}\n")).expect("APP.md");
    std::fs::write(app_dir.join("specs/model.csdl.xml"), csdl(types)).expect("CSDL");
    written::keep_app(root, app, &["app.toml", "APP.md", "specs/model.csdl.xml"]);
}

/// A tenant whose ledger owns `Entrys` and whose notes app is installed from
/// `installed_root`, with notes' next version, declaring `Note` and `Entry`,
/// in the catalog. Returns the platform, after which installing notes is the upgrade.
async fn upgrade_into(
    data_dir: &Path,
    tenant: &str,
    installed_root: impl FnOnce(&PlatformState) -> PathBuf,
) -> PlatformState {
    let ps = Arc::new(store());
    let platform = server_on(data_dir, &ps);
    let (v1, v2) = ("1".repeat(40), "2".repeat(40));
    let ledger = cache(&platform, tenant, LEDGER, &v1);
    write_declaring(&ledger, LEDGER, &["Entry"]);
    let notes = installed_root(&platform);
    for installed in [
        record(&ledger, tenant, LEDGER, &v1),
        record(&notes, tenant, NOTES, &v1),
    ] {
        ps.record_installed_app_metadata(&installed)
            .await
            .expect("record install");
    }
    register_written_apps(
        tenant,
        ledger.clone(),
        written::read(&ledger).expect("list"),
    );
    let next = cache(&platform, tenant, NOTES, &v2);
    write_declaring(&next, NOTES, &["Note", "Entry"]);
    register_written_apps(tenant, next.clone(), written::read(&next).expect("list"));
    platform
}

fn ownership_warnings(result: &crate::os_apps::InstallResult) -> Vec<&str> {
    result
        .warnings
        .iter()
        .filter(|warning| warning.check == "entity_ownership")
        .map(|warning| warning.message.as_str())
        .collect()
}

#[tokio::test]
async fn an_upgrade_that_newly_declares_a_set_another_app_owns_is_refused() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let tenant = format!("{TENANT}-upgrade-new-set");
    let platform = upgrade_into(data_dir.path(), &tenant, |platform| {
        let installed = cache(platform, &tenant, NOTES, &"1".repeat(40));
        write_declaring(&installed, NOTES, &["Note"]);
        installed
    })
    .await;

    let error = crate::os_apps::install_os_app(&platform, &tenant, NOTES)
        .await
        .expect_err("a set new to the upgrade that another app owns is refused");
    assert!(
        error.contains(&format!(
            "entity set Entrys is already owned by app '{LEDGER}', and the installed version \
             of '{NOTES}' does not declare it"
        )),
        "{error}"
    );
}

#[tokio::test]
async fn an_upgrade_keeping_a_set_its_installed_version_shares_warns() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let tenant = format!("{TENANT}-upgrade-standing-set");
    let platform = upgrade_into(data_dir.path(), &tenant, |platform| {
        let installed = cache(platform, &tenant, NOTES, &"1".repeat(40));
        write_declaring(&installed, NOTES, &["Note", "Entry"]);
        installed
    })
    .await;

    let result = crate::os_apps::install_os_app(&platform, &tenant, NOTES)
        .await
        .expect("a pair that stands from before is not refused");
    assert!(
        ownership_warnings(&result).contains(
            &format!("entity set Entrys is declared by both '{NOTES}' and '{LEDGER}'").as_str()
        ),
        "{:?}",
        result.warnings
    );
    assert!(
        !ownership_warnings(&result)
            .iter()
            .any(|warning| warning.contains("is not known")),
        "{:?}",
        result.warnings
    );
}

/// The installed version's files are gone from every cache, so whether the
/// set is new cannot be told: the upgrade is not refused on a guess, and the
/// warning says why it could not be told.
#[tokio::test]
async fn an_upgrade_whose_installed_version_cannot_be_read_warns_with_the_reason() {
    let data_dir = tempfile::tempdir().expect("data dir");
    let lost = tempfile::tempdir().expect("lost files");
    let tenant = format!("{TENANT}-upgrade-unreadable");
    let platform = upgrade_into(data_dir.path(), &tenant, |_| {
        write_declaring(lost.path(), NOTES, &["Note"]);
        lost.path().to_path_buf()
    })
    .await;

    let result = crate::os_apps::install_os_app(&platform, &tenant, NOTES)
        .await
        .expect("an unreadable installed version is not taken to declare nothing");
    let warnings = ownership_warnings(&result);
    assert!(
        warnings.contains(
            &format!("entity set Entrys is declared by both '{NOTES}' and '{LEDGER}'").as_str()
        ),
        "{warnings:?}"
    );
    assert!(
        warnings.contains(
            &format!(
                "whether entity set Entrys is new to '{NOTES}' is not known, because its \
                 installed version cannot be read: no cache directory holds its installed \
                 version acme/{NOTES}@{}",
                "1".repeat(40)
            )
            .as_str()
        ),
        "{warnings:?}"
    );
}
