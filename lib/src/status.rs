use std::collections::VecDeque;

use crate::spec::{BootEntry, BootOrder, Host, HostSpec, HostStatus, HostType, ImageStatus};
use crate::spec::{ImageReference, ImageSignature};
use anyhow::{Context, Result};
use camino::Utf8Path;
use fn_error_context::context;
use ostree::glib;
use ostree_container::OstreeImageReference;
use ostree_ext::container as ostree_container;
use ostree_ext::keyfileext::KeyFileExt;
use ostree_ext::oci_spec;
use ostree_ext::oci_spec::image::ImageConfiguration;
use ostree_ext::ostree;
use ostree_ext::sysroot::SysrootLock;

impl From<ostree_container::SignatureSource> for ImageSignature {
    fn from(sig: ostree_container::SignatureSource) -> Self {
        use ostree_container::SignatureSource;
        match sig {
            SignatureSource::OstreeRemote(r) => Self::OstreeRemote(r),
            SignatureSource::ContainerPolicy => Self::ContainerPolicy,
            SignatureSource::ContainerPolicyAllowInsecure => Self::Insecure,
        }
    }
}

impl From<ImageSignature> for ostree_container::SignatureSource {
    fn from(sig: ImageSignature) -> Self {
        use ostree_container::SignatureSource;
        match sig {
            ImageSignature::OstreeRemote(r) => SignatureSource::OstreeRemote(r),
            ImageSignature::ContainerPolicy => Self::ContainerPolicy,
            ImageSignature::Insecure => Self::ContainerPolicyAllowInsecure,
        }
    }
}

/// Fixme lower serializability into ostree-ext
fn transport_to_string(transport: ostree_container::Transport) -> String {
    match transport {
        // Canonicalize to registry for our own use
        ostree_container::Transport::Registry => "registry".to_string(),
        o => {
            let mut s = o.to_string();
            s.truncate(s.rfind(':').unwrap());
            s
        }
    }
}

impl From<OstreeImageReference> for ImageReference {
    fn from(imgref: OstreeImageReference) -> Self {
        let signature = match imgref.sigverify {
            ostree_container::SignatureSource::ContainerPolicyAllowInsecure => None,
            v => Some(v.into()),
        };
        Self {
            signature,
            transport: transport_to_string(imgref.imgref.transport),
            image: imgref.imgref.name,
        }
    }
}

impl From<ImageReference> for OstreeImageReference {
    fn from(img: ImageReference) -> Self {
        let sigverify = match img.signature {
            Some(v) => v.into(),
            None => ostree_container::SignatureSource::ContainerPolicyAllowInsecure,
        };
        Self {
            sigverify,
            imgref: ostree_container::ImageReference {
                // SAFETY: We validated the schema in kube-rs
                transport: img.transport.as_str().try_into().unwrap(),
                name: img.image,
            },
        }
    }
}

/// Parse an ostree origin file (a keyfile) and extract the targeted
/// container image reference.
fn get_image_origin(origin: &glib::KeyFile) -> Result<Option<OstreeImageReference>> {
    origin
        .optional_string("origin", ostree_container::deploy::ORIGIN_CONTAINER)
        .context("Failed to load container image from origin")?
        .map(|v| ostree_container::OstreeImageReference::try_from(v.as_str()))
        .transpose()
}

pub(crate) struct Deployments {
    pub(crate) staged: Option<ostree::Deployment>,
    pub(crate) rollback: Option<ostree::Deployment>,
    #[allow(dead_code)]
    pub(crate) other: VecDeque<ostree::Deployment>,
}

pub(crate) fn try_deserialize_timestamp(t: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    match chrono::DateTime::parse_from_rfc3339(t).context("Parsing timestamp") {
        Ok(t) => Some(t.into()),
        Err(e) => {
            tracing::warn!("Invalid timestamp in image: {:#}", e);
            None
        }
    }
}

pub(crate) fn labels_of_config(
    config: &oci_spec::image::ImageConfiguration,
) -> Option<&std::collections::HashMap<String, String>> {
    config.config().as_ref().and_then(|c| c.labels().as_ref())
}

/// Convert between a subset of ostree-ext metadata and the exposed spec API.
pub(crate) fn create_imagestatus(
    image: ImageReference,
    manifest_digest: &str,
    config: &ImageConfiguration,
) -> ImageStatus {
    let labels = labels_of_config(config);
    let timestamp = labels
        .and_then(|l| {
            l.get(oci_spec::image::ANNOTATION_CREATED)
                .map(|s| s.as_str())
        })
        .and_then(try_deserialize_timestamp);

    let version = ostree_container::version_for_config(config).map(ToOwned::to_owned);
    ImageStatus {
        image,
        version,
        timestamp,
        image_digest: manifest_digest.to_owned(),
    }
}

/// Given an OSTree deployment, parse out metadata into our spec.
#[context("Reading deployment metadata")]
fn boot_entry_from_deployment(
    sysroot: &SysrootLock,
    deployment: &ostree::Deployment,
) -> Result<BootEntry> {
    let repo = &sysroot.repo();
    let (image, cached_update, incompatible) = if let Some(origin) = deployment.origin().as_ref() {
        let incompatible = crate::utils::origin_has_rpmostree_stuff(origin);
        let (image, cached) = if incompatible {
            // If there are local changes, we can't represent it as a bootc compatible image.
            (None, None)
        } else if let Some(image) = get_image_origin(origin)? {
            let image = ImageReference::from(image);
            let csum = deployment.csum();
            let imgstate = ostree_container::store::query_image_commit(repo, &csum)?;
            let cached = imgstate.cached_update.map(|cached| {
                create_imagestatus(image.clone(), &cached.manifest_digest, &cached.config)
            });
            let imagestatus =
                create_imagestatus(image, &imgstate.manifest_digest, &imgstate.configuration);
            // We found a container-image based deployment
            (Some(imagestatus), cached)
        } else {
            // The deployment isn't using a container image
            (None, None)
        };
        (image, cached, incompatible)
    } else {
        // The deployment has no origin at all (this generally shouldn't happen)
        (None, None, false)
    };
    let r = BootEntry {
        image,
        cached_update,
        incompatible,
        pinned: deployment.is_pinned(),
        ostree: Some(crate::spec::BootEntryOstree {
            checksum: deployment.csum().into(),
            // SAFETY: The deployserial is really unsigned
            deploy_serial: deployment.deployserial().try_into().unwrap(),
        }),
    };
    Ok(r)
}

// Add a human readable output for bootc status
// image name
fn pretty_boot_entry_from_deployment(
    sysroot: &SysrootLock,
    deployment: &ostree::Deployment,
) -> Result<BootEntry> {
    let repo = &sysroot.repo();
    let (image, cached_update, incompatible) = if let Some(origin) = deployment.origin().as_ref() {
        let incompatible = crate::utils::origin_has_rpmostree_stuff(origin);
        let (image, cached) = if incompatible {
            // If there are local changes, we can't represent it as a bootc compatible image.
            (None, None)
        } else if let Some(image) = get_image_origin(origin)? {
            let image = ImageReference::from(image);
            let csum = deployment.csum();
            let imgstate = ostree_container::store::query_image_commit(repo, &csum)?;
            let cached = imgstate.cached_update.map(|cached| {
                create_imagestatus(image.clone(), &cached.manifest_digest, &cached.config)
            });
            let imagestatus =
                create_imagestatus(image, &imgstate.manifest_digest, &imgstate.configuration);
            // We found a container-image based deployment
            (Some(imagestatus), cached)
        } else {
            // The deployment isn't using a container image
            (None, None)
        };
        (image, cached, incompatible)
    } else {
        // The deployment has no origin at all (this generally shouldn't happen)
        (None, None, false)
    };
    let r = BootEntry {
        image,
        cached_update,
        incompatible,
        pinned: deployment.is_pinned(),
        ostree: Some(crate::spec::BootEntryOstree {
            checksum: deployment.csum().into(),
            // SAFETY: The deployserial is really unsigned
            deploy_serial: deployment.deployserial().try_into().unwrap(),
        }),
    };
    Ok(r)
}

impl BootEntry {
    /// Given a boot entry, find its underlying ostree container image
    pub(crate) fn query_image(
        &self,
        repo: &ostree::Repo,
    ) -> Result<Option<Box<ostree_container::store::LayeredImageState>>> {
        if self.image.is_none() {
            return Ok(None);
        }
        if let Some(checksum) = self.ostree.as_ref().map(|c| c.checksum.as_str()) {
            ostree_container::store::query_image_commit(repo, checksum).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// A variant of [`get_status`] that requires a booted deployment.
pub(crate) fn get_status_require_booted(
    sysroot: &SysrootLock,
) -> Result<(ostree::Deployment, Deployments, Host)> {
    let booted_deployment = sysroot.require_booted_deployment()?;
    let (deployments, host) = get_status(sysroot, Some(&booted_deployment), false)?;
    Ok((booted_deployment, deployments, host))
}

/// Gather the ostree deployment objects, but also extract metadata from them into
/// a more native Rust structure.
#[context("Computing status")]
pub(crate) fn get_status(
    sysroot: &SysrootLock,
    booted_deployment: Option<&ostree::Deployment>,
    pretty : bool
) -> Result<(Deployments, Host)> {
    let stateroot = booted_deployment.as_ref().map(|d| d.osname());
    let (mut related_deployments, other_deployments) = sysroot
        .deployments()
        .into_iter()
        .partition::<VecDeque<_>, _>(|d| Some(d.osname()) == stateroot);
    let staged = related_deployments
        .iter()
        .position(|d| d.is_staged())
        .map(|i| related_deployments.remove(i).unwrap());
    tracing::debug!("Staged: {staged:?}");
    // Filter out the booted, the caller already found that
    if let Some(booted) = booted_deployment.as_ref() {
        related_deployments.retain(|f| !f.equal(booted));
    }
    let rollback = related_deployments.pop_front();
    let rollback_queued = match (booted_deployment.as_ref(), rollback.as_ref()) {
        (Some(booted), Some(rollback)) => rollback.index() < booted.index(),
        _ => false,
    };
    let boot_order = if rollback_queued {
        BootOrder::Rollback
    } else {
        BootOrder::Default
    };
    tracing::debug!("Rollback queued={rollback_queued:?}");
    let other = {
        related_deployments.extend(other_deployments);
        related_deployments
    };
    let deployments = Deployments {
        staged,
        rollback,
        other,
    };

    let staged = deployments
        .staged
        .as_ref()
        .map(|d| boot_entry_from_deployment(sysroot, d))
        .transpose()
        .context("Staged deployment")?;
    let booted = booted_deployment
        .as_ref()
        .map(|d| { 
            let t = boot_entry_from_deployment(sysroot, d);
            if pretty {
                return t//bleh
            }
            return t //otherbleh

        } )
        .transpose()
        .context("Booted deployment")?;
    let rollback = deployments
        .rollback
        .as_ref()
        .map(|d| boot_entry_from_deployment(sysroot, d))
        .transpose()
        .context("Rollback deployment")?;
    let spec = staged
        .as_ref()
        .or(booted.as_ref())
        .and_then(|entry| entry.image.as_ref())
        .map(|img| HostSpec {
            image: Some(img.image.clone()),
            boot_order,
        })
        .unwrap_or_default();

    let ty = if booted
        .as_ref()
        .map(|b| b.image.is_some())
        .unwrap_or_default()
    {
        // We're only of type BootcHost if we booted via container image
        Some(HostType::BootcHost)
    } else {
        None
    };

    let mut host = Host::new(spec);
    host.status = HostStatus {
        staged,
        booted,
        rollback,
        rollback_queued,
        ty,
    };
    Ok((deployments, host))
}

/// Implementation of the `bootc status` CLI command.
#[context("Status")]
pub(crate) async fn status(opts: super::cli::StatusOpts) -> Result<()> {
    let host = if !Utf8Path::new("/run/ostree-booted").try_exists()? {
        Default::default()
    } else {
        crate::cli::prepare_for_write().await?;
        let sysroot = super::cli::get_locked_sysroot().await?;
        let booted_deployment = sysroot.booted_deployment();
        let (_deployments, host) = get_status(&sysroot, booted_deployment.as_ref(), opts.pretty)?;
        host
    };

    // If we're in JSON mode, then convert the ostree data into Rust-native
    // structures that can be serialized.
    // Filter to just the serializable status structures.
    let out = std::io::stdout();
    let mut out = out.lock();
    if opts.json {
        serde_json::to_writer(&mut out, &host).context("Writing to stdout")?;
    } else {
        serde_yaml::to_writer(&mut out, &host).context("Writing to stdout")?;
    }

    Ok(())
}

#[test]
fn test_convert_signatures() {
    use std::str::FromStr;
    let ir_unverified = &OstreeImageReference::from_str(
        "ostree-unverified-registry:quay.io/someexample/foo:latest",
    )
    .unwrap();
    let ir_ostree = &OstreeImageReference::from_str(
        "ostree-remote-registry:fedora:quay.io/fedora/fedora-coreos:stable",
    )
    .unwrap();

    let ir = ImageReference::from(ir_unverified.clone());
    assert_eq!(ir.image, "quay.io/someexample/foo:latest");
    assert_eq!(ir.signature, None);

    let ir = ImageReference::from(ir_ostree.clone());
    assert_eq!(ir.image, "quay.io/fedora/fedora-coreos:stable");
    assert_eq!(
        ir.signature,
        Some(ImageSignature::OstreeRemote("fedora".into()))
    );
}
