use anyhow::{Context as _, Result};
use http_client::{HttpClient, HttpClientWithUrl};
use semver::Version;
use serde::Deserialize;
use smol::io::AsyncReadExt;
use std::sync::Arc;

use crate::RELEASES_REPOSITORY;

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubReleaseAsset>,
}

#[derive(Deserialize)]
struct GithubReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GithubCommit {
    sha: String,
}

pub struct ReleaseAsset {
    pub version: String,
    pub url: String,
}

// The fourth component is a fork rebuild, not part of the upstream version.
fn release_number(tag: &str) -> Option<(Version, u64)> {
    let components = tag.strip_prefix("fork-v")?.split('.').collect::<Vec<_>>();
    if components.iter().any(|component| {
        component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit())
    }) {
        return None;
    }
    let [major, minor, patch, rest @ ..] = components.as_slice() else {
        return None;
    };
    let revision = match rest {
        [] => 0,
        [revision] => revision.parse().ok()?,
        _ => return None,
    };
    Some((
        Version::new(
            major.parse().ok()?,
            minor.parse().ok()?,
            patch.parse().ok()?,
        ),
        revision,
    ))
}

pub fn asset_file_name(asset: &str, os: &str, arch: &str) -> Result<String> {
    match (asset, os) {
        ("zed", "linux") => Ok(format!("endo-linux-{arch}.tar.gz")),
        ("zed", "macos") => Ok(format!("Endo-{arch}.dmg")),
        ("zed-remote-server", "linux" | "macos") => Ok(format!("zed-remote-server-{os}-{arch}.gz")),
        _ => anyhow::bail!("Endo has no {asset} release for {os}"),
    }
}

fn select_release<'a>(
    releases: &'a [GithubRelease],
    version: Option<&Version>,
    file_name: &str,
) -> Option<(&'a GithubRelease, &'a GithubReleaseAsset)> {
    releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter(|release| {
            release.assets.iter().any(|asset| {
                asset.name.starts_with("endo-linux-") || asset.name.starts_with("Endo-")
            })
        })
        .filter_map(|release| {
            let (base, revision) = release_number(&release.tag_name)?;
            if version.is_some_and(|version| *version != base) {
                return None;
            }
            let asset = release
                .assets
                .iter()
                .find(|asset| asset.name == file_name)?;
            Some(((base, revision), release, asset))
        })
        .max_by(|(left, ..), (right, ..)| left.cmp(right))
        .map(|(_, release, asset)| (release, asset))
}

pub async fn get_release_asset(
    http_client: Arc<HttpClientWithUrl>,
    version: Option<Version>,
    asset: &str,
    os: &str,
    arch: &str,
) -> Result<ReleaseAsset> {
    let file_name = asset_file_name(asset, os, arch)?;
    let version = version.map(|version| Version::new(version.major, version.minor, version.patch));
    let releases: Vec<GithubRelease> = get_github_json(
        &http_client,
        &format!("https://api.github.com/repos/{RELEASES_REPOSITORY}/releases?per_page=100"),
    )
    .await?;
    let (release, asset) = select_release(&releases, version.as_ref(), &file_name)
        .with_context(|| format!("no Endo release matching {version:?} has {file_name}"))?;
    let (base, revision) = release_number(&release.tag_name).context("invalid Endo release tag")?;
    let commit: GithubCommit = get_github_json(
        &http_client,
        &format!(
            "https://api.github.com/repos/{RELEASES_REPOSITORY}/commits/{}",
            release.tag_name
        ),
    )
    .await?;
    anyhow::ensure!(
        !commit.sha.is_empty() && commit.sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid release commit SHA"
    );
    // Assets must come from the fork, never from an upstream release or an
    // arbitrary download URL embedded in an API response.
    let expected_prefix = format!(
        "https://github.com/{RELEASES_REPOSITORY}/releases/download/{}/",
        release.tag_name
    );
    anyhow::ensure!(
        asset.browser_download_url.starts_with(&expected_prefix),
        "unexpected Endo download URL"
    );
    Ok(ReleaseAsset {
        version: format!("{base}+endo.{revision}.{}", commit.sha),
        url: asset.browser_download_url.clone(),
    })
}

pub fn newer_version(
    installed: &Version,
    fetched: Version,
    installed_sha: Option<&str>,
    installed_tag: Option<&str>,
) -> Option<Version> {
    let installed_base = Version::new(installed.major, installed.minor, installed.patch);
    let fetched_base = Version::new(fetched.major, fetched.minor, fetched.patch);
    let mut metadata = fetched.build.as_str().split('.');
    if metadata.next()? != "endo" {
        return None;
    }
    let revision = metadata.next()?.parse::<u64>().ok()?;
    let sha = metadata.next()?;
    if metadata.next().is_some() || installed_sha == Some(sha) || fetched_base < installed_base {
        return None;
    }
    if let Some((base, installed_revision)) = installed_tag.and_then(release_number)
        && (fetched_base, revision) <= (base, installed_revision)
    {
        return None;
    }
    Some(fetched)
}

async fn get_github_json<T: serde::de::DeserializeOwned>(
    http_client: &HttpClientWithUrl,
    url: &str,
) -> Result<T> {
    let mut response = http_client.get(url, Default::default(), true).await?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub release request failed: {} ({url})",
        response.status()
    );
    let mut body = Vec::new();
    response.body_mut().read_to_end(&mut body).await?;
    serde_json::from_slice(&body).with_context(|| format!("invalid GitHub response from {url}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_client::{FakeHttpClient, Response};

    fn release(tag: &str, file: &str) -> GithubRelease {
        GithubRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: vec![GithubReleaseAsset {
                name: file.into(),
                browser_download_url: format!(
                    "https://github.com/{RELEASES_REPOSITORY}/releases/download/{tag}/{file}"
                ),
            }],
        }
    }

    #[test]
    fn validates_release_tags() {
        assert_eq!(
            release_number("fork-v1.25.3"),
            Some((Version::new(1, 25, 3), 0))
        );
        assert_eq!(
            release_number("fork-v1.25.3.2"),
            Some((Version::new(1, 25, 3), 2))
        );
        for tag in [
            "v1.25.3",
            "fork-v1.25",
            "fork-v1.25.x",
            "fork-v1.25.3.x",
            "fork-v1.25.3.4.5",
            "fork-v+1.25.3",
            "fork-v1.25.3.",
        ] {
            assert!(release_number(tag).is_none(), "{tag}");
        }
    }

    #[test]
    fn selects_newest_compatible_published_release() {
        let file = "endo-linux-x86_64.tar.gz";
        let mut releases = vec![
            release("fork-v1.25.3", file),
            release("fork-v1.25.3.2", file),
            release("fork-v1.26.0", file),
            release("v9.0.0", file),
            release("fork-v1.27.0", file),
            release("fork-v1.28.0", file),
        ];
        releases[4].draft = true;
        releases[5].prerelease = true;
        assert_eq!(
            select_release(&releases, None, file).map(|(release, _)| release.tag_name.as_str()),
            Some("fork-v1.26.0")
        );
        assert_eq!(
            select_release(&releases, Some(&Version::new(1, 25, 3)), file)
                .map(|(release, _)| release.tag_name.as_str()),
            Some("fork-v1.25.3.2")
        );
        assert!(select_release(&releases, None, "Zelda-aarch64.dmg").is_none());
    }

    #[test]
    fn assets_match_packaging() -> Result<()> {
        assert_eq!(
            asset_file_name("zed", "linux", "x86_64")?,
            "endo-linux-x86_64.tar.gz"
        );
        assert_eq!(
            asset_file_name("zed", "macos", "aarch64")?,
            "Endo-aarch64.dmg"
        );
        assert_eq!(
            asset_file_name("zed-remote-server", "linux", "aarch64")?,
            "zed-remote-server-linux-aarch64.gz"
        );
        assert!(asset_file_name("zed", "windows", "x86_64").is_err());
        Ok(())
    }

    #[test]
    fn remote_servers_are_not_taken_from_pre_endo_releases() {
        let file = "zed-remote-server-linux-x86_64.gz";
        let releases = [release("fork-v1.25.3", file)];
        assert!(select_release(&releases, None, file).is_none());
    }

    #[test]
    fn updates_never_downgrade_or_repeat() -> Result<()> {
        let installed = Version::new(1, 25, 3);
        for (fetched, tag, sha, expected) in [
            (
                "1.25.3+endo.2.bbbb",
                Some("fork-v1.25.3.1"),
                Some("aaaa"),
                true,
            ),
            ("1.25.3+endo.2.bbbb", None, Some("bbbb"), false),
            (
                "1.25.3+endo.2.bbbb",
                Some("fork-v1.25.3.2"),
                Some("aaaa"),
                false,
            ),
            (
                "1.25.3+endo.1.bbbb",
                Some("fork-v1.25.3.2"),
                Some("aaaa"),
                false,
            ),
            ("1.24.0+endo.2.bbbb", None, Some("aaaa"), false),
            (
                "1.26.0+endo.0.bbbb",
                Some("fork-v1.25.3"),
                Some("aaaa"),
                true,
            ),
            ("1.26.0", None, None, false),
        ] {
            assert_eq!(
                newer_version(&installed, fetched.parse()?, sha, tag).is_some(),
                expected,
                "{fetched}"
            );
        }
        Ok(())
    }

    #[test]
    fn fetches_only_endo_assets_and_surfaces_failures() -> Result<()> {
        smol::block_on(async {
            let client = FakeHttpClient::create(|request| async move {
                let body = match request.uri().path() {
                    "/repos/somtooo/zed/releases" => {
                        r#"[{"tag_name":"fork-v1.25.3.2","draft":false,"prerelease":false,"assets":[{"name":"Endo-aarch64.dmg","browser_download_url":"https://github.com/somtooo/zed/releases/download/fork-v1.25.3.2/Endo-aarch64.dmg"}]}]"#
                    }
                    "/repos/somtooo/zed/commits/fork-v1.25.3.2" => r#"{"sha":"cafe"}"#,
                    _ => anyhow::bail!("unexpected request: {}", request.uri()),
                };
                Ok(Response::builder().status(200).body(body.into())?)
            });
            let result = get_release_asset(client, None, "zed", "macos", "aarch64").await?;
            assert_eq!(result.version, "1.25.3+endo.2.cafe");
            assert!(result.url.ends_with("/Endo-aarch64.dmg"));
            assert!(
                get_release_asset(
                    FakeHttpClient::with_404_response(),
                    None,
                    "zed",
                    "linux",
                    "x86_64"
                )
                .await
                .is_err()
            );
            Ok(())
        })
    }
}
