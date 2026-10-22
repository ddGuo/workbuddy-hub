//! 外部命令的统一入口。
//!
//! 从 GUI 进程里 spawn 控制台程序（tasklist / taskkill / powershell）时，
//! Windows 默认会为每个子进程新建一个控制台窗口。切号过程中要反复查询进程是否
//! 退出，每次都弹一下黑框，看起来像在「不停打开 cmd」。
//!
//! 这里统一加上 `CREATE_NO_WINDOW`，子进程照常运行，但不创建窗口。

use std::ffi::OsStr;
use std::process::Command;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 构造一个不弹控制台窗口的命令。
pub fn hidden(program: impl AsRef<OsStr>) -> Command {
    let mut cmd = Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// 带 `CREATE_NO_WINDOW` 执行并等待，返回是否成功。
pub fn run_hidden(program: impl AsRef<OsStr>, args: &[&str]) -> bool {
    hidden(program)
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
