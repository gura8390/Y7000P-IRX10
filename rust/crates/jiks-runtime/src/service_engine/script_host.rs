use super::*;
use crate::automation_process::{
    NativeProcessTransport, ProcessRequest, ProcessRunner, ProcessStatus,
};
use jiks_core::automation::{AutomationEnvironment, RunOptions};

pub(super) struct ScriptHost {
    runner: ProcessRunner,
    request: Option<u64>,
    cancel_waiters: Vec<u64>,
}
impl Default for ScriptHost {
    fn default() -> Self {
        Self {
            runner: ProcessRunner::new(NativeProcessTransport::new()),
            request: None,
            cancel_waiters: vec![],
        }
    }
}
impl ScriptHost {
    pub(super) fn is_busy(&self) -> bool {
        self.runner.is_busy() || self.request.is_some()
    }
    pub(super) fn cancel(&self) {
        self.runner.cancel();
    }
}
impl Engine {
    pub(super) fn request_script(&mut self, id: u64, command: &str, payload: &Value) -> Result<()> {
        if command == "script.stop" {
            if self.script.is_busy() {
                ensure!(self.script.cancel_waiters.len() < 16, "停止请求过多");
                self.script.cancel_waiters.push(id);
                self.script.cancel();
            } else {
                self.finish(id, true, "没有等待中的命令");
            }
            return Ok(());
        }
        self.writable()?;
        ensure!(!self.script.is_busy(), "上一条命令尚未实际结束");
        let path = payload
            .get("script.command")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .context("请选择可执行文件；脚本请填写解释器路径并在参数中指定脚本")?;
        ensure!(
            PathBuf::from(path).is_absolute(),
            "可执行文件必须使用完整路径"
        );
        self.script.runner.start(ProcessRequest {
            options: RunOptions {
                script_path: Some(path.into()),
                script_arguments: payload
                    .get("script.arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                run_silently: true,
                wait_until_finished: true,
                check_instance: false,
            },
            working_directory: payload
                .get("script.directory")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(str::to_owned),
            environment: AutomationEnvironment::default(),
        })?;
        self.script.request = Some(id);
        self.value("script.running", json!(true));
        self.publish_script_output("命令已提交；等待真实进程退出。停止只取消等待，不强制终止外部程序。".into());
        Ok(())
    }
    pub(super) fn poll_script(&mut self) {
        if let Some(outcome) = self.script.runner.poll() {
            let ok = outcome.status == ProcessStatus::Exited
                && outcome.exit_code == Some(0)
                && outcome.failure.is_none();
            self.value("script.running", json!(false));
            self.publish_script_output(format!(
                    "{}\r\n退出码：{:?}\r\n{}\r\n{}{}",
                    outcome.message,
                    outcome.exit_code,
                    outcome.stdout,
                    outcome.stderr,
                    outcome
                        .failure
                        .map(|s| format!("\r\n{s}"))
                        .unwrap_or_default()
                ));
            if let Some(id) = self.script.request.take() {
                self.finish(
                    id,
                    ok,
                    if ok {
                        "命令已实际退出，退出码0"
                    } else {
                        "命令未成功完成；请查看输出"
                    },
                );
            }
            for id in std::mem::take(&mut self.script.cancel_waiters) {
                self.finish(id, true, "等待与输出读取已实际清理；外部程序不会被强制终止");
            }
        }
    }
    fn publish_script_output(&self,output:String){let mut state=self.state.lock().unwrap_or_else(|p|p.into_inner());let mut collections=state.collections.as_ref().clone();collections.insert("script.output".into(),vec![Item{id:"current".into(),label:"命令输出".into(),detail:String::new(),value:json!(output)}]);state.collections=Arc::new(collections);let revision=state.values.get("script.output_revision").and_then(Value::as_u64).unwrap_or(0).wrapping_add(1);state.values.insert("script.output_revision".into(),json!(revision));state.values.remove("script.output");state.metadata_revision+=1;state.revision+=1;}
}
