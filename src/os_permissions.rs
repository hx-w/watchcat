//! Desktop permission handling is independent of provider/session recovery.
//! The service lifecycle is its only switch; no per-app or per-directory setup.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::run_main;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Status {
    pub state: String,
    pub listening_processes: usize,
    pub clicks_sent: usize,
    pub dialogs_closed: usize,
    pub last_result: Option<String>,
}

static STATUS: Mutex<Option<Status>> = Mutex::new(None);

pub fn status() -> Status {
    STATUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| Status {
            state: if cfg!(target_os = "macos") {
                "not_started"
            } else {
                "unsupported"
            }
            .into(),
            ..Status::default()
        })
}

#[cfg(target_os = "macos")]
fn update_status(update: impl FnOnce(&mut Status)) {
    update(
        STATUS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Status::default),
    );
}

pub struct Guard {
    #[cfg(target_os = "macos")]
    inner: macos::Guard,
}

pub fn start(dry_run: bool) -> Guard {
    #[cfg(not(target_os = "macos"))]
    let _ = dry_run;
    Guard {
        #[cfg(target_os = "macos")]
        inner: macos::start(dry_run),
    }
}

impl Guard {
    pub fn stop(&self) {
        #[cfg(target_os = "macos")]
        self.inner.stop();
    }
}

#[cfg(any(target_os = "macos", test))]
fn is_file_access_request(text: &str) -> bool {
    // Only a complete consent heading qualifies. Do not search arbitrary
    // descriptions or application names for a directory-related substring.
    let text = text.trim().to_lowercase();
    if text.lines().count() != 1 {
        return false;
    }
    let quoted = [('“', '”'), ('"', '"'), ('「', '」')];
    let Some(rest) = quoted.iter().find_map(|(open, close)| {
        text.strip_prefix(*open)?
            .split_once(*close)
            .map(|(_, rest)| rest.trim())
    }) else {
        return false;
    };
    if let Some(resource) = ["would like to access ", "wants to access "]
        .iter()
        .find_map(|phrase| rest.strip_prefix(phrase))
    {
        return matches!(
            resource.trim_end_matches('.'),
            "files on a removable volume"
                | "files on a network volume"
                | "a removable volume"
                | "a network volume"
                | "files in your desktop folder"
                | "files in your documents folder"
                | "files in your downloads folder"
        );
    }
    if let Some(resource) = ["想要访问", "想访问", "希望访问", "想要存取", "想存取"]
        .iter()
        .find_map(|phrase| rest.strip_prefix(phrase))
    {
        let resource = resource.trim_end_matches('。');
        return [
            "可移除宗卷上的文件",
            "可移动宗卷上的文件",
            "网络宗卷上的文件",
            "可卸除卷宗上的檔案",
            "網路卷宗上的檔案",
            "可移除宗卷上的檔案",
        ]
        .contains(&resource)
            || [
                "“桌面”文件夹中的文件",
                "“文稿”文件夹中的文件",
                "“下载”文件夹中的文件",
                "「桌面」檔案夾中的檔案",
                "「文件」檔案夾中的檔案",
                "「下載項目」檔案夾中的檔案",
            ]
            .contains(&resource);
    }
    false
}

#[cfg(any(target_os = "macos", test))]
fn is_allow_button(title: &str) -> bool {
    matches!(
        title.trim().to_lowercase().as_str(),
        "allow" | "ok" | "允许" | "允許" | "好" | "好的"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_directory_consent_from_other_permissions_and_errors() {
        for text in [
            "“node” would like to access files on a removable volume.",
            "“Terminal” would like to access files in your Downloads folder.",
            "“python3” wants to access files on a network volume.",
            "“node”想要访问可移除宗卷上的文件。",
            "“Terminal”想要访问“下载”文件夹中的文件。",
        ] {
            assert!(is_file_access_request(text), "{text}");
        }
        for text in [
            "“Zoom” would like to access the camera.",
            "“Terminal” wants to control “System Events”.",
            "“node”想要访问您的通讯录。",
            "Cannot access files on a removable volume. Allow retry?",
            "Allow this application to make changes to your computer?",
            "“Downloads Folder” would like to access the camera.",
            "“Example would like to access files on a removable volume.” would like to access the camera.",
            "“Example” would like to access the camera.\nThis app would like to access files on a removable volume.",
            "“Example” would like to access files on a removable volume. Also grant camera access.",
            "“Example想要访问下载文件夹”想要访问您的通讯录。",
            "“Example”想要访问您的通讯录。\n此应用想要访问下载文件夹中的文件。",
            "“下载工具”想要访问您的通讯录。",
            "“Example” would like to access the camera.\nChoose a folder to save recordings.",
        ] {
            assert!(!is_file_access_request(text), "{text}");
        }
        assert!(is_allow_button("允许"));
        assert!(!is_allow_button("Don't Allow"));
        assert!(!is_allow_button("不允许"));
        assert!(!is_allow_button("Allow Always"));
    }
}
