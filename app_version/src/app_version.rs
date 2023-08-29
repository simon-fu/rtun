use std::collections::HashMap;

use anyhow::{Result, Context};

// pub fn write_built_file() -> Result<()> {
//     built::write_built_file().with_context(||"write_built_file fail")?;
//     println!("cargo:rerun-if-changed=build.rs");
//     Ok(())
// }

// pub fn build_verion_brief() -> anyhow::Result<String> {
//     let src = std::env::var("CARGO_MANIFEST_DIR").unwrap();
//     let pkg_ver = std::env::var("CARGO_PKG_VERSION").unwrap();

//     let hash_or_tag = build_env_git_hash(src.as_ref())?;
//     let hash_or_tag = &hash_or_tag[..];

//     // 1.0.8.8f01411
//     Ok(format!("{}.{}",pkg_ver, hash_or_tag))
// }

// pub fn build_env_git_hash(src: &std::path::Path) -> anyhow::Result<String> {
//     let hash_or_tag = get_git_hash(src).unwrap();
//     let hash_or_tag = &hash_or_tag[..7.min(hash_or_tag.len())];
//     println!("cargo:rustc-env=VER_GIT_HASH={}", hash_or_tag);
//     Ok(hash_or_tag.to_string())
// }

pub struct AppVersion {
    pub brief: String,
    pub full: String,
}

impl AppVersion {
    pub fn try_new() -> Result<Self> {
        let src = std::env::var("CARGO_MANIFEST_DIR")
        .with_context(||"no var CARGO_MANIFEST_DIR")?;

        let pkg_ver = std::env::var("CARGO_PKG_VERSION")
            .with_context(||"no var CARGO_PKG_VERSION")?;

        let (git_branch, git_hash) = get_git_branch_and_hash(src.as_ref())?;
        
        let version_brief = {
            format!("{}.{}",pkg_ver, git_hash)
        };
        

        let envmap = EnvMap::new();

        let version_full = {

            let now = chrono::offset::Local::now();
        
            let target = envmap.get("TARGET")?;
        
            let rustc_cmd = envmap.get("RUSTC")?;
            let rustc_version = get_version_from_cmd(rustc_cmd.as_ref())?;
        
            let profile = envmap.get("PROFILE")?;
            let features = envmap.get_features();
        
            format!(
                "version [{}.{}], build at [{}], for [{}], by [{}], profile [{}], features {:?}",
                version_brief,
                git_branch, //built_info::PKG_VERSION,
                now,
                target,
                rustc_version,
                profile,
                features,
            )
        };
        
        Ok(Self {
            brief: version_brief,
            full: version_full,
        })
    }

    pub fn write_to_rustc_env(&self) {
        println!("cargo:rustc-env=APP_VER_BRIEF={}", self.brief);
        println!("cargo:rustc-env=APP_VER_FULL={}", self.full);
    }
}

fn get_version_from_cmd(executable: &std::ffi::OsStr) -> std::io::Result<String> {
    let output = std::process::Command::new(executable).arg("-V").output()?;
    let mut v = String::from_utf8(output.stdout).unwrap();
    v.pop(); // remove newline
    Ok(v)
}

fn get_git_branch_and_hash(src: &std::path::Path) -> anyhow::Result<(String, String)> {

    let env_git_branch = std::env::var("REPO_GIT_BRANCH").ok();
    let env_git_hash = std::env::var("REPO_GIT_HASH").ok();
    

    let (repo_head, repo_commit, _commit_short) = match get_repo_head(src.as_ref()) {
        Ok(Some((b, c, cs))) => (b, Some(c), Some(cs)),
        _ => (None, None, None),
    };

    let branch = match env_git_branch {
        Some(v) => v,
        None => {
            let mut branch = None;
            if let Some(x) = &repo_head {
                for next in x.split('/') {
                    branch = Some(next)
                }
            }

            branch
            .map(|x|x.to_string())
            .unwrap_or_else(||"unknown".into())
        },
    };

    let commit = if env_git_hash.is_some() {
        env_git_hash
    } else {
        repo_commit
    };

    let commit = commit
        .map(|x|(&x[..7.min(x.len())]).to_string())
        .unwrap_or_else(||"unknown".into());

    Ok((branch, commit))
}

// fn get_git_hash(src: &std::path::Path) -> anyhow::Result<String> {
//     let r = std::env::var("REPO_GIT_HASH");
//     if let Ok(h) = r {
//         if !h.is_empty() {
//             return Ok(h)
//         }
//     }

//     let (hash_or_tag, _dirty) = get_repo_description(src.as_ref())
//     .with_context(||"get_repo_description fail")?
//     .with_context(||"empty repo desc")?;
//     Ok(hash_or_tag)
// }



// fn get_repo_description(root: &std::path::Path) -> Result<Option<(String, bool)>, git2::Error> {
//     match git2::Repository::discover(root) {
//         Ok(repo) => {
//             let mut desc_opt = git2::DescribeOptions::new();
//             desc_opt.describe_tags().show_commit_oid_as_fallback(true);
//             let tag = repo
//                 .describe(&desc_opt)
//                 .and_then(|desc| desc.format(None))?;
//             let mut st_opt = git2::StatusOptions::new();
//             st_opt.include_ignored(false);
//             st_opt.include_untracked(false);
//             let dirty = repo
//                 .statuses(Some(&mut st_opt))?
//                 .iter()
//                 .any(|status| !matches!(status.status(), git2::Status::CURRENT));
//             Ok(Some((tag, dirty)))
//         }
//         Err(ref e)
//             if e.class() == git2::ErrorClass::Repository
//                 && e.code() == git2::ErrorCode::NotFound =>
//         {
//             Ok(None)
//         }
//         Err(e) => Err(e),
//     }
// }

fn get_repo_head(
    root: &std::path::Path,
) -> Result<Option<(Option<String>, String, String)>, git2::Error> {
    match git2::Repository::discover(root) {
        Ok(repo) => {
            // Supposed to be the reference pointed to by HEAD, but it's HEAD
            // itself, if detached
            let head_ref = repo.head()?;
            let branch = {
                // Check whether `head` is realy the pointed to reference and
                // not HEAD itself.
                if !repo.head_detached()? {
                    head_ref.name()
                } else {
                    None
                }
            };
            let head = head_ref.peel_to_commit()?;
            let commit = head.id();
            let commit_short = head.into_object().short_id()?;
            Ok(Some((
                branch.map(ToString::to_string),
                format!("{commit}"),
                commit_short.as_str().unwrap_or_default().to_string(),
            )))
        }
        Err(ref e)
            if e.class() == git2::ErrorClass::Repository
                && e.code() == git2::ErrorCode::NotFound =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}


pub struct EnvMap {
    envs: HashMap<String, String>,
}

impl EnvMap {
    pub fn new() -> Self {
        let mut envs = HashMap::new();
        for (k, v) in std::env::vars_os() {
            let k = k.into_string();
            let v = v.into_string();
            if let (Ok(k), Ok(v)) = (k, v) {
                envs.insert(k, v);
            }
        }

        Self {
            envs,
        }
    }

    pub fn get_features(&self) -> Vec<String> {
        let mut features = Vec::new();

        for name in self.envs.keys() {
            if let Some(feat) = name.strip_prefix("CARGO_FEATURE_") {
                features.push(feat.to_owned());
            }
        }
        features.sort();

        features
    }

    pub fn get(&self, key: &str) -> Result<&str>{
        self.envs.get(key)
        .with_context(||format!("Not found env [{}]", key))
        .map(|x|x.as_str())
    }
}




// macro_rules! print_env_var {
//     ($key:expr) => {
//         println!("dddddd {} [{:?}]", $key, std::env::var($key).unwrap());
//     }
// }

// fn print_info() {
//     // 查看调试输出： grep -rI "dddddd" target/debug/build/*

//     // src dir ["/Users/simon/simon/src/tts-rs/tts-rs"]
//     let src = std::env::var("CARGO_MANIFEST_DIR").unwrap();
//     println!("dddddd src dir [{:?}]", src); 

//     // dst dir ["/Users/simon/simon/src/tts-rs/target/debug/build/tts-f79a9fc4a424e96c/out"]
//     let dst = std::env::var("OUT_DIR").unwrap();
//     println!("dddddd dst dir [{:?}]", dst); 

//     // [Some(("8f01411", true))]
//     let r = built::util::get_repo_description(src.as_ref()).unwrap();
//     println!("dddddd repo desc [{:?}]", r); 
    
//     // [Some((Some("refs/heads/main"), "8f014111b0525a7e9d901048783e13421c674064"))]
//     let r = built::util::get_repo_head(src.as_ref()).unwrap();
//     println!("dddddd repo head [{:?}]", r); 

//     // CARGO_PKG_VERSION ["1.0.8"]
//     print_env_var!("CARGO_PKG_VERSION");

//     // TARGET ["aarch64-apple-darwin"]
//     print_env_var!("TARGET");

//     // PROFILE ["debug"]
//     print_env_var!("PROFILE");
    
// }

