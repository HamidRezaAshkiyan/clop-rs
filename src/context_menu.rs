use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// Installs or removes the "Optimize with clop-rs" right-click context menu integration
/// in **macOS Finder** (Quick Actions / Services) and **Windows Explorer** (Shell context menu).
pub fn set_context_menu(enable: bool) -> Result<PathBuf> {
    let exe_path = resolve_preferred_exe_path()?;

    #[cfg(target_os = "macos")]
    {
        let home = dirs::home_dir().context("Could not find home directory")?;
        let services_dir = home.join("Library").join("Services");
        let workflow_dir = services_dir.join("Optimize with clop-rs.workflow");

        if enable {
            let contents_dir = workflow_dir.join("Contents");
            fs::create_dir_all(&contents_dir)?;

            let info_plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSBackgroundColorName</key>
			<string>background</string>
			<key>NSIconName</key>
			<string>NSActionTemplate</string>
			<key>NSMenuItem</key>
			<dict>
				<key>default</key>
				<string>Optimize with clop-rs</string>
			</dict>
			<key>NSMessage</key>
			<string>runWorkflowAsService</string>
			<key>NSRequiredContext</key>
			<dict>
				<key>NSApplicationIdentifier</key>
				<string>com.apple.finder</string>
			</dict>
			<key>NSSendFileTypes</key>
			<array>
				<string>public.image</string>
				<string>public.movie</string>
				<string>com.adobe.pdf</string>
				<string>public.item</string>
			</array>
		</dict>
	</array>
</dict>
</plist>
"#;
            fs::write(contents_dir.join("Info.plist"), info_plist)?;

            let shell_cmd = format!(
                "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$PATH\"\n\
                 if [ -x \"/Applications/clop-rs.app/Contents/MacOS/clop-rs\" ]; then\n\
                   \"/Applications/clop-rs.app/Contents/MacOS/clop-rs\" file --hud \"$@\"\n\
                 elif [ -x \"/usr/local/bin/clop-rs\" ]; then\n\
                   \"/usr/local/bin/clop-rs\" file --hud \"$@\"\n\
                 else\n\
                   \"{}\" file --hud \"$@\"\n\
                 fi",
                exe_path.display()
            );

            let document_wflow = build_macos_document_wflow(&shell_cmd);
            fs::write(contents_dir.join("document.wflow"), document_wflow)?;

            // Refresh macOS Services / Quick Actions cache
            let _ = Command::new("/System/Library/CoreServices/pbs")
                .arg("-update")
                .status();
        } else if workflow_dir.exists() {
            fs::remove_dir_all(&workflow_dir)?;
            let _ = Command::new("/System/Library/CoreServices/pbs")
                .arg("-update")
                .status();
        }

        Ok(workflow_dir)
    }

    #[cfg(target_os = "windows")]
    {
        let reg_key = r"HKCU\Software\Classes\*\shell\clop-rs";
        let cmd_key = r"HKCU\Software\Classes\*\shell\clop-rs\command";

        if enable {
            let exe_str = exe_path.display().to_string();
            let command_val = format!("\"{}\" file --hud \"%1\"", exe_str);

            let _ = Command::new("reg")
                .args(["add", reg_key, "/ve", "/d", "Optimize with clop-rs", "/f"])
                .status();
            let _ = Command::new("reg")
                .args(["add", reg_key, "/v", "Icon", "/d", &exe_str, "/f"])
                .status();
            let _ = Command::new("reg")
                .args(["add", cmd_key, "/ve", "/d", &command_val, "/f"])
                .status();
        } else {
            let _ = Command::new("reg").args(["delete", reg_key, "/f"]).status();
        }

        Ok(PathBuf::from(reg_key))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = enable;
        Ok(exe_path)
    }
}

/// Checks whether the "Optimize with clop-rs" right-click context menu is currently installed.
pub fn is_context_menu_enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = dirs::home_dir() {
            return home
                .join("Library")
                .join("Services")
                .join("Optimize with clop-rs.workflow")
                .exists();
        }
        false
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("reg")
            .args(["query", r"HKCU\Software\Classes\*\shell\clop-rs"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        false
    }
}

fn resolve_preferred_exe_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let app_bin = PathBuf::from("/Applications/clop-rs.app/Contents/MacOS/clop-rs");
        if app_bin.exists() {
            return Ok(app_bin);
        }
        let usr_bin = PathBuf::from("/usr/local/bin/clop-rs");
        if usr_bin.exists() {
            return Ok(usr_bin);
        }
    }
    env::current_exe().context("Failed to determine current executable path")
}

#[cfg(target_os = "macos")]
fn build_macos_document_wflow(shell_script: &str) -> String {
    let escaped = shell_script
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AMApplicationBuild</key>
	<string>523</string>
	<key>AMApplicationVersion</key>
	<string>2.10</string>
	<key>AMDocumentVersion</key>
	<string>2</string>
	<key>actions</key>
	<array>
		<dict>
			<key>action</key>
			<dict>
				<key>AMAccepts</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Optional</key>
					<true/>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.path</string>
					</array>
				</dict>
				<key>AMActionVersion</key>
				<string>2.0.3</string>
				<key>AMApplication</key>
				<array>
					<string>Automator</string>
				</array>
				<key>AMParameterProperties</key>
				<dict>
					<key>COMMAND_STRING</key>
					<dict/>
					<key>CheckedForUserDefaultShell</key>
					<dict/>
					<key>inputMethod</key>
					<dict/>
					<key>shell</key>
					<dict/>
					<key>source</key>
					<dict/>
				</dict>
				<key>AMProvides</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.path</string>
					</array>
				</dict>
				<key>ActionBundlePath</key>
				<string>/System/Library/Automator/Run Shell Script.action</string>
				<key>ActionName</key>
				<string>Run Shell Script</string>
				<key>ActionParameters</key>
				<dict>
					<key>COMMAND_STRING</key>
					<string>{escaped}</string>
					<key>CheckedForUserDefaultShell</key>
					<true/>
					<key>inputMethod</key>
					<integer>1</integer>
					<key>shell</key>
					<string>/bin/zsh</string>
					<key>source</key>
					<string></string>
				</dict>
				<key>BundleIdentifier</key>
				<string>com.apple.RunShellScript</string>
				<key>CFBundleVersion</key>
				<string>2.0.3</string>
				<key>CanShowSelectedItemsWhenRun</key>
				<false/>
				<key>CanShowWhenRun</key>
				<true/>
				<key>Category</key>
				<array>
					<string>AMCategoryUtilities</string>
				</array>
				<key>Class Name</key>
				<string>RunShellScriptAction</string>
				<key>InputUUID</key>
				<string>6A91D418-4D5A-4E8B-8C9E-112233445566</string>
				<key>Keywords</key>
				<array>
					<string>Shell</string>
					<string>Script</string>
				</array>
				<key>OutputUUID</key>
				<string>7B91D418-4D5A-4E8B-8C9E-112233445566</string>
				<key>UUID</key>
				<string>8C91D418-4D5A-4E8B-8C9E-112233445566</string>
				<key>UnlocalizedApplications</key>
				<array>
					<string>Automator</string>
				</array>
			</dict>
		</dict>
	</array>
	<key>connectors</key>
	<dict/>
	<key>workflowMetaData</key>
	<dict>
		<key>ApplicationBundleID</key>
		<string>com.apple.finder</string>
		<key>ApplicationBundleIDsByPath</key>
		<dict>
			<key>/System/Library/CoreServices/Finder.app</key>
			<string>com.apple.finder</string>
		</dict>
		<key>ApplicationPath</key>
		<string>/System/Library/CoreServices/Finder.app</string>
		<key>ApplicationPaths</key>
		<array>
			<string>/System/Library/CoreServices/Finder.app</string>
		</array>
		<key>inputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>outputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>presentationMode</key>
		<integer>15</integer>
		<key>processesInput</key>
		<integer>0</integer>
		<key>serviceApplicationBundleID</key>
		<string>com.apple.finder</string>
		<key>serviceApplicationPath</key>
		<string>/System/Library/CoreServices/Finder.app</string>
		<key>serviceInputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>serviceOutputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>serviceProcessesInput</key>
		<integer>0</integer>
		<key>systemImageName</key>
		<string>NSActionTemplate</string>
		<key>useAutomaticInputType</key>
		<integer>0</integer>
		<key>workflowTypeIdentifier</key>
		<string>com.apple.Automator.servicesMenu</string>
	</dict>
</dict>
</plist>
"#
    )
}
