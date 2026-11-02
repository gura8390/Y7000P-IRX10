//! Native COM WMI; no PowerShell/.NET bridge. Each instance stays on its COM thread.
use crate::{domain::*, ownership::OwnerToken};
use anyhow::{Context, Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{collections::BTreeMap, ptr};
use windows::{
    Win32::System::{
        Ole::{
            SafeArrayAccessData, SafeArrayCreateVector, SafeArrayDestroy, SafeArrayUnaccessData,
        },
        Variant::{VARIANT, VT_ARRAY, VT_UI1, VariantClear},
        Wmi::IWbemClassObject,
    },
    core::{BSTR, HSTRING},
};
use wmi::{COMLibrary, Variant, WMIConnection};

thread_local! { static COM:std::cell::RefCell<Option<COMLibrary>>=const{std::cell::RefCell::new(None)}; }
fn thread_com() -> Result<COMLibrary> {
    COM.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(COMLibrary::new()?);
        }
        Ok(slot.unwrap())
    })
}

struct OwnedVariant(VARIANT);
impl Default for OwnedVariant {
    fn default() -> Self {
        Self(VARIANT::default())
    }
}
impl Drop for OwnedVariant {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub enum MethodInput {
    U32(u32),
    U64(u64),
    I32(i32),
    Byte(u8),
    Bool(bool),
    String(String),
    Bytes(Vec<u8>),
}
impl MethodInput {
    fn variant(&self, cim_type: i32) -> Result<OwnedVariant> {
        let value = match self {
            Self::U32(v) => {
                ensure!(
                    cim_type == 19 || cim_type == 3,
                    "Expected CIM_UINT32/SINT32"
                );
                if cim_type == 3 {
                    ensure!(*v <= i32::MAX as u32, "Signed CIM input overflow");
                    Variant::I4(*v as i32)
                } else {
                    // CIM uint32 uses VT_I4 on the WMI wire; retain all 32 bits.
                    Variant::I4(*v as i32)
                }
            }
            Self::U64(v) => {
                ensure!(cim_type == 21, "Expected CIM_UINT64");
                // IWbemClassObject::Put specifies a decimal VT_BSTR for uint64.
                Variant::String(v.to_string())
            }
            Self::I32(v) => {
                ensure!(cim_type == 3, "Expected CIM_SINT32");
                Variant::I4(*v)
            }
            Self::Byte(v) => {
                ensure!(cim_type == 17, "Expected CIM_UINT8");
                Variant::UI1(*v)
            }
            Self::Bool(v) => {
                ensure!(cim_type == 11, "Expected CIM_BOOLEAN");
                Variant::Bool(*v)
            }
            Self::String(v) => {
                ensure!(cim_type == 8, "Expected CIM_STRING");
                Variant::String(v.clone())
            }
            Self::Bytes(bytes) => {
                ensure!(
                    cim_type == (0x2000 | 17),
                    "Expected CIM_UINT8[] input for fan table"
                );
                ensure!(
                    !bytes.is_empty() && bytes.len() <= 65536,
                    "Invalid byte-array size"
                );
                let array = unsafe { SafeArrayCreateVector(VT_UI1, 0, bytes.len() as u32) };
                ensure!(!array.is_null(), "SAFEARRAY allocation failed");
                let mut data = ptr::null_mut();
                if let Err(e) = unsafe { SafeArrayAccessData(array, &mut data) } {
                    unsafe {
                        let _ = SafeArrayDestroy(array);
                    }
                    return Err(e.into());
                }
                unsafe {
                    ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());
                }
                if let Err(e) = unsafe { SafeArrayUnaccessData(array) } {
                    unsafe {
                        let _ = SafeArrayDestroy(array);
                    }
                    return Err(e.into());
                }
                let mut result = OwnedVariant::default();
                unsafe {
                    (*result.0.Anonymous.Anonymous).vt = VT_ARRAY | VT_UI1;
                    (*result.0.Anonymous.Anonymous).Anonymous.parray = array;
                }
                return Ok(result);
            }
        };
        Ok(OwnedVariant(value.try_into()?))
    }
}

fn get_property(object: &IWbemClassObject, name: &str) -> Result<(Variant, i32)> {
    let mut value = OwnedVariant::default();
    let mut cim_type = 0;
    unsafe {
        object.Get(
            &HSTRING::from(name),
            0,
            &mut value.0,
            Some(&mut cim_type),
            None,
        )?;
    }
    let result = Variant::from_variant(&value.0)?
        .convert_into_cim_type(windows::Win32::System::Wmi::CIMTYPE_ENUMERATION(cim_type))?;
    Ok((result, cim_type))
}

fn owned_json(value: Variant) -> Result<Value> {
    // Reject embedded COM objects instead of leaking their apartment-bound identity into UI DTOs.
    match value {
        Variant::Empty | Variant::Null => Ok(Value::Null),
        Variant::Array(values) => Ok(Value::Array(
            values.into_iter().map(owned_json).collect::<Result<_>>()?,
        )),
        Variant::Unknown(_) | Variant::Object(_) => {
            bail!("Embedded COM objects require an explicit typed adapter")
        }
        value => Ok(serde_json::to_value(value)?),
    }
}

pub fn check_method_return(cim_type: Option<i32>, value: Option<&Value>) -> Result<()> {
    match (cim_type, value) {
        (None, None) => Ok(()),
        (Some(11), Some(Value::Bool(true))) => Ok(()),
        (Some(3 | 16 | 17 | 18 | 19 | 20 | 21 | 2), Some(v))
            if v.as_i64() == Some(0) || v.as_u64() == Some(0) =>
        {
            Ok(())
        }
        _ => bail!(
            "Firmware returned failure or wrong return type: CIM {cim_type:?}, value {value:?}"
        ),
    }
}

pub struct WmiFacade {
    connection: WMIConnection,
}
impl WmiFacade {
    pub(crate) fn subscribe_fixed(
        &self,
        kind: crate::firmware_events::FirmwareEventKind,
    ) -> Result<windows::Win32::System::Wmi::IEnumWbemClassObject> {
        use windows::Win32::System::Wmi::{
            IWbemContext, WBEM_FLAG_FORWARD_ONLY, WBEM_FLAG_RETURN_IMMEDIATELY,
        };
        Ok(unsafe {
            self.connection.svc.ExecNotificationQuery(
                &BSTR::from("WQL"),
                &BSTR::from(format!("SELECT * FROM {}", kind.class())),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None::<&IWbemContext>,
            )?
        })
    }
    pub(crate) fn event_value(object: &IWbemClassObject, field: &str) -> Result<Value> {
        owned_json(get_property(object, field)?.0)
    }
    pub fn connect(namespace: &str) -> Result<Self> {
        ensure!(
            matches!(
                namespace.to_ascii_uppercase().as_str(),
                "ROOT\\WMI" | "ROOT\\CIMV2" | "ROOT\\CIMV2\\POWER"
            ),
            "Namespace outside the local hardware facade"
        );
        Ok(Self {
            connection: WMIConnection::with_namespace_path(namespace, thread_com()?)?,
        })
    }
    pub fn query<T: DeserializeOwned>(&self, query: &str) -> Result<Vec<T>> {
        ensure!(
            query
                .trim_start()
                .to_ascii_uppercase()
                .starts_with("SELECT "),
            "Only read-only SELECT queries are accepted"
        );
        let rows = self.connection.exec_query_native_wrapper(query)?;
        let mut result = Vec::new();
        for row in rows {
            let row = row?;
            let mut values = serde_json::Map::new();
            for name in row.list_properties()? {
                let (value, _) = get_property(&row.inner, &name)?;
                values.insert(name, owned_json(value)?);
            }
            // __PATH is explicitly requested by a method caller, not part of a normal query result.
            result.push(serde_json::from_value(Value::Object(values))?);
        }
        Ok(result)
    }
    fn single_instance(&self, class: &str) -> Result<IWbemClassObject> {
        ensure!(
            class.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "Invalid WMI class name"
        );
        let mut rows = self
            .connection
            .exec_query_native_wrapper(format!("SELECT * FROM {class}"))?;
        let first = rows.next().context("No firmware instance was returned")??;
        ensure!(
            rows.next().is_none(),
            "Expected exactly one firmware instance"
        );
        Ok(first.inner)
    }
    pub(crate) fn method_read(
        &self,
        class: &str,
        method: &str,
        parameters: BTreeMap<String, MethodInput>,
    ) -> Result<BTreeMap<String, Value>> {
        ensure!(
            method.starts_with("Get") || method.starts_with("Is") || method.starts_with("Fan_Get"),
            "A read-only method cannot invoke a setter"
        );
        self.method(class, method, parameters)
    }
    pub(crate) fn method_write(
        &self,
        _owner: &OwnerToken,
        action: &ExplicitAction,
        class: &str,
        method: &str,
        parameters: BTreeMap<String, MethodInput>,
    ) -> Result<BTreeMap<String, Value>> {
        self.method_guarded(
            class,
            method,
            parameters,
            Some(&mut || action.check_write_boundary()),
        )
    }
    pub(crate) fn method_write_guarded(
        &self,
        _owner: &OwnerToken,
        action: &ExplicitAction,
        class: &str,
        method: &str,
        parameters: BTreeMap<String, MethodInput>,
        mut guard: impl FnMut() -> Result<()>,
    ) -> Result<BTreeMap<String, Value>> {
        self.method_guarded(
            class,
            method,
            parameters,
            Some(&mut || {
                action.check_write_boundary()?;
                guard()?;
                action.check_deadline()
            }),
        )
    }
    /// Read-only method metadata preflight, with no ExecMethod call.
    pub(crate) fn validate_inputs(
        &self,
        class: &str,
        method: &str,
        parameters: &BTreeMap<String, MethodInput>,
    ) -> Result<()> {
        ensure!(
            !class.is_empty()
                && class.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !method.is_empty()
                && method
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "Invalid WMI method identifier"
        );
        let mut definition = None;
        unsafe {
            self.connection.svc.GetObject(
                &BSTR::from(class),
                Default::default(),
                None::<&windows::Win32::System::Wmi::IWbemContext>,
                Some(&mut definition),
                None,
            )?;
        }
        let definition = definition.context("WMI class definition missing")?;
        let (mut input, mut output) = (None, None);
        unsafe {
            definition.GetMethod(&HSTRING::from(method), 0, &mut input, &mut output)?;
        }
        if let Some(signature) = input {
            for (name, value) in parameters {
                let (_, kind) = get_property(&signature, name)?;
                let _owned = value.variant(kind)?;
            }
        } else {
            ensure!(parameters.is_empty(), "WMI method has no input signature");
        }
        Ok(())
    }
    fn method(
        &self,
        class: &str,
        method: &str,
        parameters: BTreeMap<String, MethodInput>,
    ) -> Result<BTreeMap<String, Value>> {
        self.method_guarded(class, method, parameters, None)
    }
    fn method_guarded(
        &self,
        class: &str,
        method: &str,
        parameters: BTreeMap<String, MethodInput>,
        mut guard: Option<&mut dyn FnMut() -> Result<()>>,
    ) -> Result<BTreeMap<String, Value>> {
        ensure!(
            method
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "Invalid WMI method name"
        );
        let instance = self.single_instance(class)?;
        let (path, _) = get_property(&instance, "__PATH")?;
        let Variant::String(path) = path else {
            bail!("Firmware instance lacks __PATH")
        };
        let mut definition = None;
        unsafe {
            self.connection.svc.GetObject(
                &BSTR::from(class),
                Default::default(),
                None::<&windows::Win32::System::Wmi::IWbemContext>,
                Some(&mut definition),
                None,
            )?;
        }
        let definition = definition.context("WMI class definition missing")?;
        let mut input_signature = None;
        let mut output_signature = None;
        unsafe {
            definition.GetMethod(
                &HSTRING::from(method),
                0,
                &mut input_signature,
                &mut output_signature,
            )?;
        }
        let input = if let Some(signature) = input_signature {
            let instance = unsafe { signature.SpawnInstance(0)? };
            for (name, value) in parameters {
                let (_, cim_type) = get_property(&signature, &name)?;
                let value = value.variant(cim_type)?;
                unsafe {
                    instance.Put(&HSTRING::from(name), 0, &value.0, cim_type)?;
                }
            }
            Some(instance)
        } else {
            ensure!(parameters.is_empty(), "WMI method has no input signature");
            None
        };
        // Distinguish a genuinely absent ReturnValue from failure to read its
        // metadata. A metadata error must never silently become success.
        let return_type = if let Some(signature) = &output_signature {
            let wrapper = wmi::result_enumerator::IWbemClassWrapper::new(signature.clone());
            if let Some(name) = wrapper
                .list_properties()?
                .into_iter()
                .find(|n| n.eq_ignore_ascii_case("ReturnValue"))
            {
                Some(get_property(signature, &name)?.1)
            } else {
                None
            }
        } else {
            None
        };
        let mut output = None;
        if let Some(guard) = guard.as_mut() {
            guard()?;
        }
        unsafe {
            self.connection.svc.ExecMethod(
                &BSTR::from(path),
                &BSTR::from(method),
                Default::default(),
                None::<&windows::Win32::System::Wmi::IWbemContext>,
                input.as_ref(),
                Some(&mut output),
                None,
            )?;
        }
        let mut result = BTreeMap::new();
        if let Some(output) = output {
            let wrapper = wmi::result_enumerator::IWbemClassWrapper::new(output);
            for name in wrapper.list_properties()? {
                let (value, _) = get_property(&wrapper.inner, &name)?;
                result.insert(name, owned_json(value)?);
            }
        }
        check_output_return(class, method, return_type, &result)?;
        Ok(result)
    }
}

fn check_output_return(
    class: &str,
    method: &str,
    cim_type: Option<i32>,
    values: &BTreeMap<String, Value>,
) -> Result<()> {
    // Lenovo's documented BIOS interface returns string "Success" in `return`.
    // This exception is deliberately limited to three fixed BIOS methods. It
    // must never make a generic string-returning firmware setter successful.
    // https://docs.lenovocdrt.com/ref/bios/wmi/wmi_guide/#return-values
    let bios = [
        ("Lenovo_GetBiosSelections", "GetBiosSelections"),
        ("Lenovo_SetBiosSetting", "SetBiosSetting"),
        ("Lenovo_SaveBiosSettings", "SaveBiosSettings"),
    ]
    .into_iter()
    .any(|(c, m)| class.eq_ignore_ascii_case(c) && method.eq_ignore_ascii_case(m));
    if bios {
        let returns = values
            .iter()
            .filter(|(key, _)| {
                key.eq_ignore_ascii_case("return") || key.eq_ignore_ascii_case("ReturnValue")
            })
            .collect::<Vec<_>>();
        ensure!(
            returns.len() == 1,
            "BIOS method did not provide exactly one documented return status"
        );
        ensure!(
            cim_type.is_none() || cim_type == Some(8),
            "BIOS ReturnValue metadata is not CIM_STRING"
        );
        ensure!(
            returns[0].1.as_str() == Some("Success"),
            "BIOS method rejected the request: {}",
            returns[0].1
        );
        return Ok(());
    }
    check_method_return(
        cim_type,
        values
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("ReturnValue"))
            .map(|(_, v)| v),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bios_string_status_is_exact_and_limited_to_documented_methods() {
        let mut v = BTreeMap::from([("return".into(), Value::from("Success"))]);
        assert!(check_output_return("Lenovo_SetBiosSetting", "SetBiosSetting", None, &v).is_ok());
        v.insert("return".into(), Value::from("Access Denied"));
        assert!(check_output_return("Lenovo_SetBiosSetting", "SetBiosSetting", None, &v).is_err());
        assert!(
            check_output_return(
                "Lenovo_SaveBiosSettings",
                "SaveBiosSettings",
                None,
                &BTreeMap::new()
            )
            .is_err()
        );
        let v = BTreeMap::from([("ReturnValue".into(), Value::from("Success"))]);
        assert!(
            check_output_return("LENOVO_OTHER_METHOD", "SetFeatureValue", Some(8), &v).is_err()
        );
        assert!(
            check_output_return("Lenovo_GetBiosSelections", "GetBiosSelections", Some(8), &v)
                .is_ok()
        );
        assert!(
            check_output_return(
                "Lenovo_GetBiosSelections",
                "GetBiosSelections",
                Some(11),
                &v
            )
            .is_err()
        );
    }
    #[test]
    fn bool_success_is_not_an_integer_error_code() {
        assert!(check_method_return(Some(11), Some(&Value::Bool(true))).is_ok());
        assert!(check_method_return(Some(11), Some(&Value::Bool(false))).is_err());
        assert!(check_method_return(Some(19), Some(&Value::from(0))).is_ok());
        assert!(check_method_return(Some(19), Some(&Value::from(1))).is_err());
        assert!(check_method_return(Some(11), Some(&Value::from(0))).is_err());
        assert!(check_method_return(Some(19), None).is_err());
    }
    #[test]
    fn byte_array_uses_native_ui1_safearray_and_rejects_wrong_metadata() {
        assert!(MethodInput::Bytes(vec![1, 2, 3]).variant(19).is_err());
        let bytes = vec![0, 1, 127, 128, 255];
        let value = MethodInput::Bytes(bytes.clone())
            .variant(0x2000 | 17)
            .unwrap();
        let observed = Variant::from_variant(&value.0).unwrap();
        assert_eq!(
            observed,
            Variant::Array(bytes.into_iter().map(Variant::UI1).collect())
        );
    }
    #[test]
    fn wmi_integer_wire_types_preserve_unsigned_values() {
        assert_eq!(
            Variant::from_variant(&MethodInput::U32(u32::MAX).variant(19).unwrap().0).unwrap(),
            Variant::I4(-1)
        );
        assert!(MethodInput::U32(u32::MAX).variant(3).is_err());
        assert_eq!(
            Variant::from_variant(&MethodInput::U64(u64::MAX).variant(21).unwrap().0).unwrap(),
            Variant::String(u64::MAX.to_string())
        );
    }
}
