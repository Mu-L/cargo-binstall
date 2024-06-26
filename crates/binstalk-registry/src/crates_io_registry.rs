use binstalk_downloader::remote::{Client, Error as RemoteError, Url};
use binstalk_types::cargo_toml_binstall::Meta;
use cargo_toml_workspace::cargo_toml::Manifest;
use compact_str::{CompactString, ToCompactString};
use semver::{Comparator, Op as ComparatorOp, Version as SemVersion, VersionReq};
use serde::Deserialize;
use tracing::{debug, instrument};

use crate::{parse_manifest, MatchedVersion, RegistryError};

/// Return `Some(checksum)` if the version is not yanked, otherwise `None`.
async fn is_crate_yanked(client: &Client, url: Url) -> Result<Option<String>, RemoteError> {
    #[derive(Deserialize)]
    struct CrateInfo {
        version: Inner,
    }

    #[derive(Deserialize)]
    struct Inner {
        yanked: bool,
        checksum: String,
    }

    // Fetch / update index
    debug!("Looking up crate information");

    let info: CrateInfo = client.get(url).send(true).await?.json().await?;
    let version = info.version;

    Ok((!version.yanked).then_some(version.checksum))
}

async fn fetch_crate_cratesio_version_matched(
    client: &Client,
    url: Url,
    version_req: &VersionReq,
) -> Result<Option<(CompactString, String)>, RemoteError> {
    #[derive(Deserialize)]
    struct CrateInfo {
        #[serde(rename = "crate")]
        inner: CrateInfoInner,

        versions: Vec<Version>,
    }

    #[derive(Deserialize)]
    struct CrateInfoInner {
        max_stable_version: CompactString,
    }

    #[derive(Deserialize)]
    struct Version {
        num: CompactString,
        yanked: bool,
        checksum: String,
    }

    // Fetch / update index
    debug!("Looking up crate information");

    let crate_info: CrateInfo = client.get(url).send(true).await?.json().await?;

    let version_with_checksum = if version_req == &VersionReq::STAR {
        let version = crate_info.inner.max_stable_version;
        crate_info
            .versions
            .into_iter()
            .find_map(|v| (v.num.as_str() == version.as_str()).then_some(v.checksum))
            .map(|checksum| (version, checksum))
    } else {
        crate_info
            .versions
            .into_iter()
            .filter_map(|item| {
                if !item.yanked {
                    // Remove leading `v` for git tags
                    let num = if let Some(num) = item.num.strip_prefix('v') {
                        num.into()
                    } else {
                        item.num
                    };

                    // Parse out version
                    let ver = semver::Version::parse(&num).ok()?;

                    // Filter by version match
                    version_req
                        .matches(&ver)
                        .then_some((num, ver, item.checksum))
                } else {
                    None
                }
            })
            // Return highest version
            .max_by(
                |(_ver_str_x, ver_x, _checksum_x), (_ver_str_y, ver_y, _checksum_y)| {
                    ver_x.cmp(ver_y)
                },
            )
            .map(|(ver_str, _, checksum)| (ver_str, checksum))
    };

    Ok(version_with_checksum)
}

/// Find the crate by name, get its latest stable version matches `version_req`,
/// retrieve its Cargo.toml and infer all its bins.
#[instrument(
    skip(client),
    fields(
        version_req = format_args!("{version_req}"),
    )
)]
pub async fn fetch_crate_cratesio_api(
    client: Client,
    name: &str,
    version_req: &VersionReq,
) -> Result<Manifest<Meta>, RegistryError> {
    let url = Url::parse(&format!("https://crates.io/api/v1/crates/{name}"))?;

    let (version, cksum) = match version_req.comparators.as_slice() {
        [Comparator {
            op: ComparatorOp::Exact,
            major,
            minor: Some(minor),
            patch: Some(patch),
            pre,
        }] => {
            let version = SemVersion {
                major: *major,
                minor: *minor,
                patch: *patch,
                pre: pre.clone(),
                build: Default::default(),
            }
            .to_compact_string();

            let mut url = url.clone();
            url.path_segments_mut().unwrap().push(&version);

            is_crate_yanked(&client, url)
                .await
                .map(|ret| ret.map(|checksum| (version, checksum)))
        }
        _ => fetch_crate_cratesio_version_matched(&client, url.clone(), version_req).await,
    }
    .map_err(|e| match e {
        RemoteError::Http(e) if e.is_status() => RegistryError::NotFound(name.into()),
        e => e.into(),
    })?
    .ok_or_else(|| RegistryError::VersionMismatch {
        req: version_req.clone(),
    })?;

    debug!("Found information for crate version: '{version}'");

    // Download crate to temporary dir (crates.io or git?)
    let mut crate_url = url;
    crate_url
        .path_segments_mut()
        .unwrap()
        .push(&version)
        .push("download");

    parse_manifest(client, name, crate_url, MatchedVersion { version, cksum }).await
}
