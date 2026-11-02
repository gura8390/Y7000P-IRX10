//! Read-only inventory for the original device-information hierarchy. These
//! queries contain no temperatures, firmware setters, GPU library initialization
//! or user-account fields. Serial fields remain local and masked by the UI.
use crate::{devices,wmi_native::WmiFacade,monotonic_ms};
use anyhow::Result;
use serde_json::{Value,json};
fn show(value:&Value)->String{match value{Value::Null=>"—".into(),Value::String(s)=>s.trim().to_owned(),_=>value.to_string()}}
fn size(value:&Value)->String{let bytes=value.as_u64().or_else(||value.as_str().and_then(|s|s.parse().ok()));bytes.map(|n|format!("{:.1} GiB",n as f64/1073741824.)).unwrap_or_else(||show(value))}
fn group(facade:&WmiFacade,id:&str,title:&str,query:&str,fields:&[(&str,&str,bool,bool)])->Value{
    let result:Result<Vec<Value>>=facade.query(query);
    match result{
        Ok(items)=>{let mut rows=Vec::new();for(index,item)in items.iter().take(128).enumerate(){if items.len()>1{rows.push(json!({"label":format!("{} {}",title,index+1),"value":"","heading":true}));}
            for(key,label,bytes,sensitive)in fields{if let Some(value)=item.get(*key).filter(|v|!v.is_null()){rows.push(json!({"label":label,"value":if *bytes{size(value)}else{show(value)},"sensitive":sensitive}));}}
        }json!({"id":id,"title":title,"rows":rows,"available":true})},
        Err(error)=>json!({"id":id,"title":title,"rows":[],"available":false,"error":format!("{error:#}")}),
    }
}
pub fn read()->Result<Value>{
    let requested_at_tick_ms=monotonic_ms();let facade=WmiFacade::connect("ROOT\\CIMV2")?;
    let mut groups=vec![
        group(&facade,"device","设备","SELECT Manufacturer,Model,SystemFamily,SystemSKUNumber FROM Win32_ComputerSystem",&[("Manufacturer","制造商",false,false),("Model","型号",false,false),("SystemFamily","系列",false,false),("SystemSKUNumber","机型代号",false,false)]),
        group(&facade,"system","系统","SELECT Caption,Version,BuildNumber,OSArchitecture,InstallDate,LastBootUpTime FROM Win32_OperatingSystem",&[("Caption","Windows",false,false),("Version","版本",false,false),("BuildNumber","内部版本",false,false),("OSArchitecture","架构",false,false),("InstallDate","安装日期",false,false),("LastBootUpTime","最近启动",false,false)]),
        group(&facade,"cpu","CPU","SELECT Name,Manufacturer,NumberOfCores,NumberOfLogicalProcessors,MaxClockSpeed,SocketDesignation FROM Win32_Processor",&[("Name","处理器",false,false),("NumberOfCores","核心数",false,false),("NumberOfLogicalProcessors","线程数",false,false),("MaxClockSpeed","额定最高频率 · MHz",false,false),("SocketDesignation","插槽",false,false)]),
        group(&facade,"gpu","显卡","SELECT Name,AdapterRAM,DriverVersion,DriverDate,VideoProcessor FROM Win32_VideoController",&[("Name","显卡",false,false),("AdapterRAM","WMI 驱动报告显存",true,false),("DriverVersion","驱动版本",false,false),("DriverDate","驱动日期",false,false),("VideoProcessor","图形处理器",false,false)]),
        group(&facade,"memory","内存","SELECT Manufacturer,PartNumber,Capacity,Speed,ConfiguredClockSpeed,DeviceLocator,SerialNumber FROM Win32_PhysicalMemory",&[("Manufacturer","厂商",false,false),("PartNumber","料号",false,false),("Capacity","容量",true,false),("Speed","标称速率 · MT/s",false,false),("ConfiguredClockSpeed","当前速率 · MT/s",false,false),("DeviceLocator","插槽",false,false),("SerialNumber","序列号",false,true)]),
        group(&facade,"disks","磁盘","SELECT Model,Size,InterfaceType,FirmwareRevision,SerialNumber FROM Win32_DiskDrive",&[("Model","型号",false,false),("Size","容量",true,false),("InterfaceType","接口",false,false),("FirmwareRevision","固件",false,false),("SerialNumber","序列号",false,true)]),
        group(&facade,"motherboard","主板","SELECT Manufacturer,Product,Version,SerialNumber FROM Win32_BaseBoard",&[("Manufacturer","制造商",false,false),("Product","型号",false,false),("Version","版本",false,false),("SerialNumber","序列号",false,true)]),
        group(&facade,"bios","BIOS","SELECT Manufacturer,SMBIOSBIOSVersion,ReleaseDate,SerialNumber FROM Win32_BIOS",&[("Manufacturer","制造商",false,false),("SMBIOSBIOSVersion","BIOS 版本",false,false),("ReleaseDate","发布日期",false,false),("SerialNumber","设备序列号",false,true)]),
        group(&facade,"network","网络","SELECT Name,NetConnectionID,MACAddress,Speed,NetConnectionStatus FROM Win32_NetworkAdapter WHERE PhysicalAdapter=TRUE",&[("Name","适配器",false,false),("NetConnectionID","连接",false,false),("MACAddress","MAC 地址",false,true),("Speed","链路速率 · bit/s",false,false),("NetConnectionStatus","连接状态代码",false,false)]),
        group(&facade,"volumes","存储卷","SELECT Name,VolumeName,FileSystem,Size,FreeSpace FROM Win32_LogicalDisk WHERE DriveType=3",&[("Name","驱动器",false,false),("VolumeName","卷标",false,false),("FileSystem","文件系统",false,false),("Size","总容量",true,false),("FreeSpace","可用容量",true,false)]),
    ];
    match devices::displays(){Ok(displays)=>{let mut rows=Vec::new();for display in displays{rows.push(json!({"label":display.description,"value":display.current.map(|m|format!("{} × {} · {} Hz",m.width,m.height,m.hertz)).unwrap_or_else(||"当前模式不可用".into())}));}groups.push(json!({"id":"displays","title":"显示器","available":true,"rows":rows}));},Err(error)=>groups.push(json!({"id":"displays","title":"显示器","available":false,"error":format!("{error:#}"),"rows":[]}))}
    Ok(json!({"groups":groups,"requested_at_tick_ms":requested_at_tick_ms,"completed_at_tick_ms":monotonic_ms(),"read_only":true,"uploads":false}))
}
#[cfg(test)]mod tests{use super::*;#[test]fn metadata_formatter_does_not_truncate_u64_sizes_or_include_accounts(){assert_eq!(size(&json!(17179869184u64)),"16.0 GiB");assert_eq!(show(&Value::Null),"—");}}
