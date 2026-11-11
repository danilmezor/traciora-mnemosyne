//! Production owning-realm adapter. All property access and invocation executes
//! in the actual VM; roots and original thrown values retain ownership.
use crate::live_metadata_types::{WidgetRealm, WidgetRoot, WidgetValue};
use mnemosyne_runtime::{NodeChild, NodeHostDispatcher, RootLease, RootedHostError};
use mnemosyne_runtime_protocol::{
    HostError, HostKey, HostValue, JsReference, ObjectInspection, ObjectQuery,
};

#[derive(Clone, Debug)]
pub struct Root(pub RootLease);
impl WidgetRoot for Root {
    fn reference(&self) -> JsReference {
        self.0.reference()
    }
}
pub struct Realm<'a> {
    pub child: &'a mut NodeChild,
    pub context: u64,
    pub dispatcher: Option<&'a mut dyn NodeHostDispatcher>,
}
impl Realm<'_> {
    fn error(&mut self, error: HostError) -> RootedHostError {
        match self.child.retain_error(error) {
            Ok(error) => error,
            Err(error) => RootedHostError {
                error,
                roots: Vec::new(),
            },
        }
    }
    pub fn query(&mut self, query: ObjectQuery) -> Result<ObjectInspection, RootedHostError> {
        let result = match self.dispatcher.as_deref_mut() {
            Some(dispatcher) => self.child.inspect_with_dispatcher(query, dispatcher),
            None => self.child.inspect_host(query),
        };
        result.map_err(|error| self.error(error))
    }
    fn invalid(&mut self, message: &str) -> RootedHostError {
        self.error(HostError::unsupported(message))
    }
    pub fn value(&mut self, query: ObjectQuery) -> Result<HostValue, RootedHostError> {
        match self.query(query)? {
            ObjectInspection::Value(value) => Ok(value),
            _ => Err(self.invalid("VM operation did not return a value")),
        }
    }
    pub fn root(&mut self, value: HostValue) -> Result<Root, RootedHostError> {
        let HostValue::Reference(reference) = value else {
            return Err(self.invalid("VM operation did not return a reference"));
        };
        let lease = self
            .child
            .retain_reference(reference)
            .map_err(|error| self.error(error))?;
        // Transfer this temporary query's borrowed ownership to the lease.
        self.child
            .release_reference(reference.id)
            .map_err(|error| self.invalid(&error.to_string()))?;
        Ok(Root(lease))
    }
    /// A callback envelope has one borrowed base per identity, even when the
    /// same object appears as receiver and several arguments. Promote each
    /// identity once before any VM operation can consume/reacquire its base.
    pub fn callback_values(
        &mut self,
        values: impl IntoIterator<Item = HostValue>,
    ) -> Result<Vec<WidgetValue<Root>>, RootedHostError> {
        let mut roots = std::collections::HashMap::new();
        let mut result = Vec::new();
        for value in values {
            if let HostValue::Reference(reference) = &value {
                if let Some(owned) = roots.get(&reference.id) {
                    result.push(WidgetValue::Reference(Root::clone(owned)));
                    continue;
                }
            }
            let owned = self.retain(value)?;
            if let WidgetValue::Reference(root) = &owned {
                roots.insert(root.reference().id, root.clone());
            }
            result.push(owned);
        }
        Ok(result)
    }
    pub fn global(&mut self, name: &str) -> Result<WidgetValue<Root>, RootedHostError> {
        let value = self.value(ObjectQuery::GlobalValue(name.into()))?;
        self.retain(value)
    }
    #[allow(dead_code)] // Shared adapter; used by the independent timing target.
    pub fn date_now(&mut self) -> Result<f64, RootedHostError> {
        let date = self.global("Date")?;
        let now = self.member(&date, "now")?;
        match self.invoke(&now, &date, Vec::new())? {
            WidgetValue::Number(value) => Ok(value),
            _ => Err(self
                .invalid("Date.now returned a nonnumber (global override branch not implemented)")),
        }
    }
    #[allow(dead_code)] // Shared adapter; used by the independent timing target.
    pub fn call_global(
        &mut self,
        name: &str,
        arguments: Vec<HostValue>,
    ) -> Result<WidgetValue<Root>, RootedHostError> {
        let function = self.global(name)?;
        self.invoke(&function, &WidgetValue::Undefined, arguments)
    }
    pub fn member(
        &mut self,
        receiver: &WidgetValue<Root>,
        name: &str,
    ) -> Result<WidgetValue<Root>, RootedHostError> {
        self.get(receiver, &name.encode_utf16().collect::<Vec<_>>())
    }
    pub fn invoke(
        &mut self,
        function: &WidgetValue<Root>,
        receiver: &WidgetValue<Root>,
        arguments: Vec<HostValue>,
    ) -> Result<WidgetValue<Root>, RootedHostError> {
        let value = self.value(ObjectQuery::Invoke {
            function: function.host_value(),
            receiver: receiver.host_value(),
            arguments,
        })?;
        self.retain(value)
    }
    pub fn stringify_value(
        &mut self,
        value: &WidgetValue<Root>,
    ) -> Result<Vec<u16>, RootedHostError> {
        let json = self.global("JSON")?;
        let function = self.member(&json, "stringify")?;
        let value = self.invoke(
            &function,
            &json,
            vec![
                value.host_value(),
                HostValue::Undefined,
                HostValue::Number(2.0),
            ],
        )?;
        match value {
            WidgetValue::Utf16(text) => Ok(text),
            _ => Err(self.invalid("JSON.stringify returned a nonstring value")),
        }
    }
}
impl WidgetRealm for Realm<'_> {
    type Root = Root;
    type Error = RootedHostError;
    fn is_javascript_exception(error: &Self::Error) -> bool {
        error.error.thrown.is_some()
    }
    fn context(&self) -> u64 {
        self.context
    }
    fn validate_root(&mut self, root: &Root) -> Result<(), RootedHostError> {
        self.child
            .validate_lease(&root.0)
            .map_err(|error| self.error(error))
    }
    fn retain(&mut self, value: HostValue) -> Result<WidgetValue<Root>, RootedHostError> {
        Ok(match value {
            HostValue::Undefined => WidgetValue::Undefined,
            HostValue::Null => WidgetValue::Null,
            HostValue::Boolean(value) => WidgetValue::Boolean(value),
            HostValue::Number(value) => WidgetValue::Number(value),
            HostValue::Utf16(value) => WidgetValue::Utf16(value),
            HostValue::BigInt { negative, words } => WidgetValue::BigInt { negative, words },
            value @ HostValue::Reference(_) => WidgetValue::Reference(self.root(value)?),
            value => {
                let value = self.value(ObjectQuery::Materialize(value))?;
                return self.retain(value);
            }
        })
    }
    fn ordinary_object(&mut self) -> Result<Root, RootedHostError> {
        let value = self.value(ObjectQuery::Materialize(HostValue::Record(Vec::new())))?;
        self.root(value)
    }
    fn get(
        &mut self,
        receiver: &WidgetValue<Root>,
        key: &[u16],
    ) -> Result<WidgetValue<Root>, RootedHostError> {
        let value = self.value(ObjectQuery::GetValue(
            receiver.host_value(),
            HostKey::Utf16(key.to_vec()),
        ))?;
        self.retain(value)
    }
    fn set(
        &mut self,
        receiver: &WidgetValue<Root>,
        key: &[u16],
        value: &WidgetValue<Root>,
    ) -> Result<(), RootedHostError> {
        self.query(ObjectQuery::SetValue {
            receiver: receiver.host_value(),
            key: HostKey::Utf16(key.to_vec()),
            value: value.host_value(),
        })?;
        Ok(())
    }
    fn set_computed(
        &mut self,
        receiver: &Root,
        key: &WidgetValue<Root>,
        value: &WidgetValue<Root>,
    ) -> Result<(), RootedHostError> {
        self.query(ObjectQuery::SetComputed {
            receiver: receiver.0.value(),
            key: key.host_value(),
            value: value.host_value(),
        })?;
        Ok(())
    }
    fn define_data(
        &mut self,
        receiver: &Root,
        key: &[u16],
        value: &WidgetValue<Root>,
    ) -> Result<(), RootedHostError> {
        self.query(ObjectQuery::DefineData {
            receiver: receiver.0.value(),
            key: HostKey::Utf16(key.to_vec()),
            value: value.host_value(),
        })?;
        Ok(())
    }
    fn delete(&mut self, receiver: &Root, key: &[u16]) -> Result<(), RootedHostError> {
        self.query(ObjectQuery::Delete {
            receiver: receiver.0.value(),
            key: HostValue::Utf16(key.to_vec()),
        })?;
        Ok(())
    }
    fn own_enumerable_keys(&mut self, object: &Root) -> Result<Vec<Vec<u16>>, RootedHostError> {
        match self.query(ObjectQuery::OwnEnumerableKeys(object.0.value()))? {
            ObjectInspection::Keys(keys) => keys
                .into_iter()
                .map(|key| match key {
                    HostKey::Utf16(key) => Ok(key),
                    _ => Err(self.invalid("Enumerable key inspection included a symbol")),
                })
                .collect(),
            _ => Err(self.invalid("Enumerable key inspection did not return keys")),
        }
    }
    fn begin_for_in(&mut self, object: &Root) -> Result<Root, RootedHostError> {
        let value = self.value(ObjectQuery::BeginForIn(object.0.value()))?;
        self.root(value)
    }
    fn next_for_in(&mut self, iterator: &Root) -> Result<Option<Vec<u16>>, RootedHostError> {
        match self.query(ObjectQuery::NextForIn(iterator.reference().id))? {
            ObjectInspection::ForInKey(key) => Ok(key),
            _ => Err(self.invalid("for-in iterator did not return a key/done result")),
        }
    }
    fn has_own(&mut self, object: &Root, key: &[u16]) -> Result<bool, RootedHostError> {
        let constructor = self.global("Object")?;
        let has_own = self.member(&constructor, "hasOwn")?;
        match self.invoke(
            &has_own,
            &constructor,
            vec![object.0.value(), HostValue::Utf16(key.to_vec())],
        )? {
            WidgetValue::Boolean(value) => Ok(value),
            _ => Err(self.invalid("Object.hasOwn returned a nonboolean")),
        }
    }
    fn invoke_has_own(&mut self, object: &Root, key: &[u16]) -> Result<bool, RootedHostError> {
        let receiver = WidgetValue::Reference(object.clone());
        let has_own = self.member(&receiver, "hasOwnProperty")?;
        match self.invoke(&has_own, &receiver, vec![HostValue::Utf16(key.to_vec())])? {
            WidgetValue::Boolean(value) => Ok(value),
            _ => Err(self.invalid("hasOwnProperty returned a nonboolean")),
        }
    }
    fn stringify(&mut self, object: &Root) -> Result<Vec<u16>, RootedHostError> {
        self.stringify_value(&WidgetValue::Reference(object.clone()))
    }
}
