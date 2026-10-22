//! A function in a binary

use uuid::Uuid;

/// A single disassembled instruction from a function
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct CompiledInstruction {
    /// The address this instruction starts at
    pub address: u64,
    /// The contents of this instruction
    pub instruction: String,
}

/// A compiled function
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct CompiledFunction {
    /// The address this function is located at
    pub address: u64,
    /// The dissasembled instructions for this function
    pub disassembly: Vec<CompiledInstruction>,
}

impl CompiledFunction {
    /// Add this compiled functions entity metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // always set our entity kind
        let mut form = form
            .text("kind", super::EntityKinds::CompiledFunction.as_str())
            .text("metadata[function_address]", self.address.to_string());
        // add this functions metadata (list fields require a trailing `[]`)
        crate::multipart_list_serialize!(form, "metadata[disassembly][]", self.disassembly);
        Ok(form)
    }

    /// Create a new compiled function entity with the info in the form
    ///
    /// # Arguments
    ///
    /// * `form` -  The update form
    #[cfg(feature = "api")]
    pub fn from_form(form: super::EntityMetadataForm) -> Result<Self, crate::utils::ApiError> {
        // if we don't have the address set then return an error
        let address = match form.function_address {
            Some(address) => address,
            None => {
                return crate::bad!("Compiled function entities must have a address!".to_owned());
            }
        };
        // build incident entity
        Ok(CompiledFunction {
            address,
            disassembly: form.disassembly,
        })
    }
}

/// The decompilation for a function
#[derive(Debug, Clone, Default, Serialize, Deserialize, Hash)]
#[serde(from = "DecompiledFunctionCompat")]
#[cfg_attr(feature = "api", derive(utoipa::ToSchema))]
pub struct DecompiledFunction {
    /// The address this descompiled function is located at
    pub address: u64,
    /// The tool instances that decompiled this function
    pub tools: Vec<Uuid>,
    /// The names of tools that decompiled this function that are not linked to a tool instance
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub legacy_tools: Vec<String>,
    /// The decompiled function
    pub content: String,
}

/// The stored shape of a decompiled function, accepting tool names written before tool instances
#[derive(Debug, Clone, Default, Deserialize)]
struct DecompiledFunctionCompat {
    /// The address this descompiled function is located at
    address: u64,
    /// The tools that decompiled this function as tool instance IDs or legacy tool names
    #[serde(default)]
    tools: Vec<String>,
    /// The names of tools that are not linked to a tool instance
    #[serde(default)]
    legacy_tools: Vec<String>,
    /// The decompiled function
    content: String,
}

impl From<DecompiledFunctionCompat> for DecompiledFunction {
    /// Convert a stored decompiled function into a decompiled function entity
    ///
    /// # Arguments
    ///
    /// * `compat` - The stored decompiled function
    fn from(compat: DecompiledFunctionCompat) -> Self {
        // split the stored tools into tool instances and legacy names
        let (tools, legacy_tools) =
            super::shared::split_tool_refs(compat.tools, compat.legacy_tools);
        DecompiledFunction {
            address: compat.address,
            tools,
            legacy_tools,
            content: compat.content,
        }
    }
}

impl DecompiledFunction {
    /// Get the form fields this decompiled function sends, by name
    ///
    /// Legacy tool names are never sent, since no form writes them.
    #[must_use]
    pub fn form_fields(&self) -> Vec<(&'static str, String)> {
        // the kind, address, and content of this function
        let mut fields = vec![
            (
                "kind",
                super::EntityKinds::DecompiledFunction.as_str().to_owned(),
            ),
            ("metadata[function_address]", self.address.to_string()),
            ("metadata[decompilation_content]", self.content.clone()),
        ];
        // the tool instances that decompiled this function
        fields.extend(
            self.tools
                .iter()
                .map(|tool| ("metadata[tool_ids][]", tool.to_string())),
        );
        fields
    }

    /// Add this decompiled functions entity metadata to a form
    ///
    /// # Arguments
    ///
    /// * `form` - The form to add too
    #[cfg(feature = "client")]
    pub fn add_to_form(
        self,
        form: reqwest::multipart::Form,
    ) -> Result<reqwest::multipart::Form, crate::Error> {
        // add each of our fields to the form
        let form = self
            .form_fields()
            .into_iter()
            .fold(form, |form, (name, value)| form.text(name, value));
        Ok(form)
    }

    /// Create a new compiled function entity with the info in the form
    ///
    /// # Arguments
    ///
    /// * `form` -  The update form
    #[cfg(feature = "api")]
    pub fn from_form(form: super::EntityMetadataForm) -> Result<Self, crate::utils::ApiError> {
        // if we don't have the address set then return an error
        let address = match form.function_address {
            Some(address) => address,
            None => {
                return crate::bad!("Decompiled function entities must have a address!".to_owned());
            }
        };
        // if we don't have any decompilation content set then return an error
        let content = match form.decompilation_content {
            Some(content) => content,
            None => {
                return crate::bad!(
                    "Decompiled function entities must have decompilation content!".to_owned()
                );
            }
        };
        // record each tool instance once in the order it was sent
        let mut tools = Vec::with_capacity(form.tool_ids.len());
        super::shared::add_tool_ids(&mut tools, form.tool_ids);
        // legacy tool names are never written by a form
        Ok(DecompiledFunction {
            address,
            tools,
            legacy_tools: Vec::new(),
            content,
        })
    }
}

/// Tests for reading, writing, and sending decompiled function entities
#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::DecompiledFunction;

    /// A stored decompiled function without tools reads with no tool references at all
    #[test]
    fn compat_reads_missing_and_empty_tools() {
        // read a row without a tools key and a row with empty tools
        for raw in [
            r#"{"address":16,"content":"x"}"#,
            r#"{"address":16,"tools":[],"content":"x"}"#,
        ] {
            // read this row
            let func: DecompiledFunction = serde_json::from_str(raw).unwrap();
            // neither list has anything in it
            assert!(func.tools.is_empty(), "{raw}");
            assert!(func.legacy_tools.is_empty(), "{raw}");
        }
    }

    /// A decompiled function stored with tool instances and legacy names reads back unchanged
    #[test]
    fn compat_reads_new_shape_unchanged() {
        // build a row in the current shape
        let id = Uuid::new_v4();
        let raw =
            format!(r#"{{"address":16,"tools":["{id}"],"legacy_tools":["ida"],"content":"x"}}"#);
        // read the row
        let func: DecompiledFunction = serde_json::from_str(&raw).unwrap();
        // both lists are what was stored
        assert_eq!(func.tools, vec![id]);
        assert_eq!(func.legacy_tools, vec!["ida".to_owned()]);
    }

    /// A decompiled function serializes and deserializes unchanged, and an empty `legacy_tools`
    /// is left out
    #[test]
    fn new_shape_round_trips() {
        // build a function with legacy names and one without
        let with_legacy = DecompiledFunction {
            address: 32,
            tools: vec![Uuid::new_v4()],
            legacy_tools: vec!["ida".to_owned()],
            content: "x".to_owned(),
        };
        let without_legacy = DecompiledFunction {
            legacy_tools: Vec::new(),
            ..with_legacy.clone()
        };
        // round trip each of them through JSON
        for func in [with_legacy, without_legacy] {
            // serialize this function
            let value = serde_json::to_value(&func).unwrap();
            // an empty legacy list isn't written
            assert_eq!(
                value.get("legacy_tools").is_some(),
                !func.legacy_tools.is_empty()
            );
            // the tool instances are written as uuid strings
            assert_eq!(
                value["tools"],
                serde_json::json!([func.tools[0].to_string()])
            );
            // reading it back gives the same function
            let read: DecompiledFunction = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(&read).unwrap(), value);
        }
    }

    /// A decompiled function request sends its tool instances as `tool_ids` and never sends tool
    /// names
    #[test]
    fn form_fields_send_tool_ids() {
        // build a function with a tool instance and a legacy name
        let id = Uuid::new_v4();
        let func = DecompiledFunction {
            address: 32,
            tools: vec![id],
            legacy_tools: vec!["ida".to_owned()],
            content: "x".to_owned(),
        };
        // the kind, address, content, and tool instance are sent and nothing else
        assert_eq!(
            func.form_fields(),
            vec![
                ("kind", "DecompiledFunction".to_owned()),
                ("metadata[function_address]", "32".to_owned()),
                ("metadata[decompilation_content]", "x".to_owned()),
                ("metadata[tool_ids][]", id.to_string()),
            ]
        );
    }
}
