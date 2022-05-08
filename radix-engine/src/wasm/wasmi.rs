use super::constants::*;
use super::errors::*;
use super::traits::*;
use scrypto::values::ScryptoValue;
use wasmi::*;

#[derive(Clone)]
pub struct WasmiScryptoModule {
    pub module_ref: ModuleRef, // TODO: make fields private
    pub memory_ref: MemoryRef,
}

pub struct WasmiEngine<T: ScryptoRuntime> {
    runtime: T,
}

pub struct WasmiEnvModule;

impl ModuleImportResolver for WasmiEnvModule {
    fn resolve_func(&self, field_name: &str, signature: &Signature) -> Result<FuncRef, Error> {
        match field_name {
            ENGINE_FUNCTION_NAME => {
                if signature.params() != [ValueType::I32, ValueType::I32, ValueType::I32]
                    || signature.return_type() != Some(ValueType::I32)
                {
                    return Err(Error::Instantiation(
                        "Function signature does not match".into(),
                    ));
                }
                Ok(FuncInstance::alloc_host(
                    signature.clone(),
                    ENGINE_FUNCTION_INDEX,
                ))
            }
            _ => Err(Error::Instantiation(format!(
                "Export {} not found",
                field_name
            ))),
        }
    }
}

impl ScryptoModule for WasmiScryptoModule {
    fn invoke_export(
        &self,
        name: &str,
        args: &[ScryptoValue],
    ) -> Result<Option<ScryptoValue>, InvokeError> {
        todo!()
    }

    fn function_exports(&self) -> Vec<String> {
        self.module_ref
            .exports()
            .iter()
            .filter(|(_, val)| matches!(val, ExternVal::Func(_)))
            .map(|(name, _)| name.to_string())
            .collect()
    }
}

impl<T: ScryptoRuntime> WasmiEngine<T> {
    pub fn new(runtime: T) -> Self {
        Self { runtime }
    }
}

impl<T: ScryptoRuntime> ScryptoWasmValidator for WasmiEngine<T> {
    fn validate(&mut self, code: &[u8]) -> Result<(), WasmValidationError> {
        // parse wasm module
        let module = Module::from_buffer(code).map_err(|_| WasmValidationError::FailedToParse)?;

        // check floating point
        module
            .deny_floating_point()
            .map_err(|_| WasmValidationError::FloatingPointNotAllowed)?;

        // Instantiate
        let instance = ModuleInstance::new(
            &module,
            &ImportsBuilder::new().with_resolver("env", &WasmiEnvModule),
        )
        .map_err(|_| WasmValidationError::FailedToInstantiate)?;

        // Check start function
        if instance.has_start() {
            return Err(WasmValidationError::StartFunctionNotAllowed);
        }
        let module_ref = instance.assert_no_start();

        // Check memory export
        match module_ref.export_by_name(EXPORT_MEMORY) {
            Some(ExternVal::Memory(_)) => {}
            _ => {
                return Err(WasmValidationError::NoMemoryExport);
            }
        }

        // Check scrypto abi
        match module_ref.export_by_name(EXPORT_SCRYPTO_ALLOC) {
            Some(ExternVal::Func(_)) => {}
            _ => {
                return Err(WasmValidationError::NoScryptoAllocExport);
            }
        }
        match module_ref.export_by_name(EXPORT_SCRYPTO_FREE) {
            // TODO: check if this is indeed needed
            Some(ExternVal::Func(_)) => {}
            _ => {
                return Err(WasmValidationError::NoScryptoFreeExport);
            }
        }

        Ok(())
    }
}

impl<T: ScryptoRuntime> ScryptoWasmExecutor<WasmiScryptoModule> for WasmiEngine<T> {
    fn instantiate(&mut self, code: &[u8]) -> WasmiScryptoModule {
        // parse wasm
        let module = Module::from_buffer(code).expect("Failed to parse wasm module");

        // link with env module
        let module_ref = ModuleInstance::new(
            &module,
            &ImportsBuilder::new().with_resolver(EXPORT_ENV, &WasmiEnvModule),
        )
        .expect("Failed to instantiate wasm module")
        .assert_no_start();

        // find memory ref
        let memory_ref = match module_ref.export_by_name(EXPORT_MEMORY) {
            Some(ExternVal::Memory(memory)) => memory,
            _ => panic!("Failed to find memory export"),
        };

        WasmiScryptoModule {
            module_ref,
            memory_ref,
        }
    }
}