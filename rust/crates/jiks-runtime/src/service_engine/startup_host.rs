use super::*;
pub(super) struct Job{id:u64,worker:std::thread::JoinHandle<Result<crate::startup_arguments::Snapshot>>}
impl Engine{
    pub(super) fn request_startup_arguments(&mut self,id:u64,command:&str,payload:&Value)->Result<()>{
        ensure!(self.startup_job.is_none()&&self.shutdown.is_none(),"启动参数文件操作尚未完成或正在退出");
        let root=self.repository.root().to_owned();let save=command=="startup.arguments.save";
        let preferences=if save{Some(serde_json::from_value::<crate::startup_arguments::Preferences>(payload.clone())?)}else{None};
        let worker=std::thread::Builder::new().name("jiks-startup-preferences".into()).spawn(move||{if let Some(preferences)=preferences{crate::startup_arguments::save(&root,&preferences)}else{Ok(crate::startup_arguments::load(&root)?.snapshot())}})?;
        self.startup_job=Some(Job{id,worker});Ok(())
    }
    pub(super) fn poll_startup_arguments(&mut self){let Some(job)=self.startup_job.take()else{return;};if !job.worker.is_finished(){self.startup_job=Some(job);return;}match job.worker.join().map_err(|_|anyhow::anyhow!("启动参数文件后台异常")).and_then(|result|result){Ok(snapshot)=>{self.value("startup.arguments",serde_json::to_value(snapshot).expect("bounded scalar startup snapshot"));self.finish(job.id,true,"启动参数文件已实际核对；保存的两项只在下一次普通启动生效，当前运行模式不变");},Err(error)=>self.finish(job.id,false,format!("启动参数未完成，原文件保留：{error:#}"))}}
}
