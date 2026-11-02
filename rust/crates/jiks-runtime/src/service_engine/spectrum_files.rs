//! Import/export operates on a draft only. It never chooses a HID interface or
//! applies a profile. The message-loop owner waits for the actual file worker.
use super::*;
use std::io::Read;
pub(super) struct FileJob {id:u64,import:bool,worker:std::thread::JoinHandle<Result<Value>>}
fn validate(raw:&Value)->Result<()> {let effects:Vec<jiks_platform::spectrum::Effect>=serde_json::from_value(raw.clone())?;ensure!(effects.len()<=255,"Spectrum灯效过多");jiks_platform::spectrum::Profile{profile:1,effects}.encode_24_zone()?;Ok(())}
fn file_operation(import:bool,path:&std::path::Path,effects:&Value)->Result<Value>{
    ensure!(path.is_absolute(),"请填写绝对文件路径");
    if import {
        let mut bytes=Vec::new();std::fs::File::open(path)?.take(1024*1024+1).read_to_end(&mut bytes)?;ensure!(bytes.len()<=1024*1024,"Spectrum文件超过1MiB限制");
        let text=std::str::from_utf8(&bytes)?.trim_start_matches('\u{feff}');let raw=JsonNode::parse(text)?.to_value()?;validate(&raw)?;Ok(raw)
    }else{
        validate(effects)?;let bytes=serde_json::to_vec_pretty(effects)?;ensure!(bytes.len()<=1024*1024,"Spectrum文件超过1MiB限制");
        let parent=path.parent().context("文件目录缺失")?;let name=path.file_name().and_then(|name|name.to_str()).context("文件名无效")?;
        let store=jiks_store::SettingsStore::new(parent)?;store.write(name,&bytes)?;ensure!(store.read(name)?.as_deref()==Some(bytes.as_slice()),"Spectrum导出文件读回不一致");Ok(json!({"bytes":bytes.len()}))
    }
}
impl Engine {
    pub(super) fn request_spectrum_file(&mut self,id:u64,command:&str,payload:&Value)->Result<()> {
        ensure!(self.spectrum_file.is_none()&&self.shutdown.is_none(),"上一个文件操作尚未实际完成");
        let path=PathBuf::from(payload.get("path").and_then(Value::as_str).context("请填写文件路径")?);ensure!(path.is_absolute(),"请填写绝对文件路径");
        let import=command=="keyboard.effects.import";let effects=payload.get("effects").cloned().unwrap_or(Value::Null);
        if !import{validate(&effects)?;}
        let worker=std::thread::Builder::new().name("jiks-spectrum-file".into()).spawn(move||file_operation(import,&path,&effects))?;
        self.spectrum_file=Some(FileJob{id,import,worker});Ok(())
    }
    pub(super) fn poll_spectrum_file(&mut self){
        if self.spectrum_import_deadline!=0&&(monotonic_ms()>=self.spectrum_import_deadline||self.shutdown.is_some()){self.clear_spectrum_import();}
        let Some(job)=self.spectrum_file.take()else{return;};if !job.worker.is_finished(){self.spectrum_file=Some(job);return;}
        let result=job.worker.join().map_err(|_|anyhow::anyhow!("Spectrum文件后台异常")).and_then(|result|result);
        match result {Ok(value)=>{
            if self.shutdown.is_some()||self.faulted{self.finish(job.id,false,"文件操作已实际返回；退出期间未导入草稿，导出文件若已写入请核对");return;}
            if job.import{
                let mut state=self.state.lock().unwrap_or_else(|p|p.into_inner());let mut collections=state.collections.as_ref().clone();collections.insert("keyboard.effects.imported".into(),vec![Item{id:job.id.to_string(),label:"导入灯效草稿".into(),detail:"仅草稿；尚未应用到键盘".into(),value:json!({"request_id":job.id,"effects":value})}]);state.collections=Arc::new(collections);state.metadata_revision+=1;state.revision+=1;drop(state);self.spectrum_import_deadline=monotonic_ms().saturating_add(60_000);
            }
            self.finish(job.id,true,if job.import{"灯效文件已读取到草稿；尚未应用到键盘"}else{"灯效文件已原子导出并读回核对"});
        },Err(error)=>self.finish(job.id,false,format!("灯效文件操作未完成：{error:#}"))}
    }
    fn clear_spectrum_import(&mut self){let mut state=self.state.lock().unwrap_or_else(|p|p.into_inner());let mut collections=state.collections.as_ref().clone();if collections.remove("keyboard.effects.imported").is_some(){state.collections=Arc::new(collections);state.metadata_revision+=1;state.revision+=1;}self.spectrum_import_deadline=0;}
    pub(super) fn acknowledge_spectrum_import(&mut self,payload:&Value)->Result<()>{let id=payload.get("request_id").and_then(Value::as_u64).context("缺少导入草稿回执ID")?;let matches=self.state.lock().unwrap_or_else(|p|p.into_inner()).collections.get("keyboard.effects.imported").and_then(|items|items.first()).is_some_and(|item|item.id==id.to_string());ensure!(matches,"过期或错误的导入草稿回执");self.clear_spectrum_import();Ok(())}
}
#[cfg(test)]mod tests{use super::*;
fn fixture()->PathBuf{static NEXT:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(0);let root=std::env::temp_dir().join(format!("JiksSpectrumFile-{}-{}",std::process::id(),NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed)));std::fs::create_dir_all(&root).unwrap();root}
fn effects()->Value{json!([{"Type":"Always","Keys":[1,24],"Colors":[{"R":255,"G":0,"B":0}],"FutureExtension":{"keep":17}}])}
#[test]fn local_file_roundtrip_preserves_unknown_effect_fields_and_never_applies_hardware(){let root=fixture();let path=root.join("effects.json");file_operation(false,&path,&effects()).unwrap();assert_eq!(file_operation(true,&path,&Value::Null).unwrap(),effects());std::fs::remove_dir_all(root).unwrap();}
#[test]fn invalid_effects_never_replace_existing_export(){let root=fixture();let path=root.join("effects.json");std::fs::write(&path,b"original").unwrap();assert!(file_operation(false,&path,&json!([{"Type":"AuroraSync"}])).is_err());assert_eq!(std::fs::read(&path).unwrap(),b"original");std::fs::remove_dir_all(root).unwrap();}
}
