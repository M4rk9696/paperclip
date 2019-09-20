use super::object::{ApiObject, ObjectField, OpRequirement, Parameter};
use super::state::{ChildModule, EmitterState};
use crate::error::PaperClipError;
use crate::v2::{
    im::ArcRwLock,
    models::{
        self, Api, Coder, CollectionFormat, DataType, DataTypeFormat, HttpMethod, Items, Operation,
        OperationMap, ParameterIn, SchemaRepr, JSON_CODER, JSON_MIME, YAML_CODER, YAML_MIME,
    },
    Schema,
};
use failure::Error;
use heck::{CamelCase, SnekCase};
use itertools::Itertools;
use url::Host;

use std::collections::HashSet;
use std::fmt::Debug;
use std::fs;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Some "thing" emitted by the emitter.
#[derive(Debug)]
pub enum EmittedUnit {
    /// Some Rust type.
    Known(String),
    /// Object represented as a Rust struct.
    Objects(Vec<ApiObject>),
    /// We've identified the Rust type, but then we also have a
    /// bunch of generated Rust structs. This happens in the
    /// presence of anonymously defined objects.
    KnownButAnonymous(String, Vec<ApiObject>),
    /// Nothing to do.
    None,
}

impl EmittedUnit {
    #[inline]
    fn known_type(&self) -> String {
        match self {
            EmittedUnit::Known(ref s) => s.clone(),
            EmittedUnit::KnownButAnonymous(ref s, _) => s.clone(),
            _ => panic!("Emitted unit {:?} is not a known type", self),
        }
    }

    #[inline]
    fn map_known(self, ty: impl Into<String>) -> Self {
        match self {
            EmittedUnit::Known(_) => EmittedUnit::Known(ty.into()),
            EmittedUnit::KnownButAnonymous(_, o) => EmittedUnit::KnownButAnonymous(ty.into(), o),
            _ => panic!("Cannot map unknown emitted units"),
        }
    }
}

/// Context for building definitions.
#[derive(Debug, Clone, Default)]
pub struct DefinitionContext<'a> {
    /// Whether we're planning to define the Rust type or whether we're
    /// reusing an existing type.
    pub define: bool,
    /// Names of parents. In JSON schema, object types are allowed to
    /// define a new object in their schema without '$ref'erencing them
    /// from elsewhere. This means, we'll have an anonymous object. So,
    /// we make use of parents (object names or property names) to
    /// autogenerate struct names at will.
    pub parents: Vec<&'a str>,
}

impl<'a> DefinitionContext<'a> {
    /// Specify whether the object needs to be defined.
    pub fn define(mut self, define: bool) -> Self {
        self.define = define;
        self
    }

    /// Creates a new context by appending the immediate parent's name.
    pub fn add_parent(mut self, parent: &'a str) -> Self {
        self.parents.push(parent);
        DefinitionContext {
            define: self.define,
            parents: self.parents,
        }
    }
}

/// `Emitter` represents the interface for generating the relevant
/// modules, API object definitions and the associated calls.
pub trait Emitter: Sized {
    /// The associated `Schema` implementor.
    type Definition: Schema + Debug;

    /* MARK: Overridable methods */

    /// Returns a reference to the underlying state.
    fn state(&self) -> &EmitterState;

    /// Returns an iterator of path components for the given definition.
    ///
    /// **NOTE:** All components are [snake_cased](https://docs.rs/heck/*/heck/trait.SnekCase.html)
    /// (including the definition name).
    fn def_ns_name<'a>(
        &self,
        def: &'a Self::Definition,
    ) -> Result<Box<dyn Iterator<Item = String> + 'a>, Error> {
        let state = self.state();
        def.name()
            .map(|n| n.split(state.ns_sep).map(SnekCase::to_snek_case))
            .ok_or_else(|| {
                trace!("Missing name for definition: {:?}", def);
                PaperClipError::InvalidDefinitionName.into()
            })
            .map(|i| Box::new(i) as Box<_>)
    }

    /// Returns the [CamelCase](https://docs.rs/heck/*/heck/trait.CamelCase.html)
    /// name for the given definition.
    fn def_name(&self, def: &Self::Definition) -> Result<String, Error> {
        Ok(self
            .def_ns_name(def)?
            .last()
            .map(|s| s.to_camel_case())
            .expect("last item always exists for split?"))
    }

    /// Returns the [CamelCase](https://docs.rs/heck/*/heck/trait.CamelCase.html)
    /// name for some definition based on its parent names. This is called whenever
    /// a definition doesn't have a name (i.e., through `$ref`) and we have to generate it.
    fn def_anon_name(&self, def: &Self::Definition, parents: &[&str]) -> Option<String> {
        let mut name = String::new();
        parents.iter().for_each(|s| {
            name.push_str(s);
            name.push_str("_");
        });

        if name.is_empty() {
            trace!("Unable to get name for anonymous schema: {:?}", def);
            None
        } else {
            Some(name.to_camel_case())
        }
    }

    /// Returns the module path (from working directory) for the given definition.
    ///
    /// **NOTE:** This should set `.rs` extension to the leaf path component.
    fn def_mod_path(&self, def: &Self::Definition) -> Result<PathBuf, Error> {
        let state = self.state();
        let mut path = state.working_dir.clone();
        path.extend(self.def_ns_name(def)?);
        path.set_extension("rs");
        Ok(path)
    }

    /// Called whenever we encounter an operation that can't be added to
    /// any modules. By default, this returns `miscellaneous.rs` module in root.
    ///
    /// **NOTE:** This should set `.rs` extension to the leaf path component.
    fn unknown_op_mod_path(
        &self,
        path: &str,
        method: HttpMethod,
        op: &Operation<SchemaRepr<Self::Definition>>,
    ) -> Result<PathBuf, Error> {
        let _ = (path, method, op);
        let state = self.state();
        let mut path = state.working_dir.clone();
        path.push("miscellaneous");
        path.set_extension("rs");
        Ok(path)
    }

    /// Called whenever we don't have an object for the module path returned by
    /// `Emitter::unknown_op_mod_path` method. By default, this returns an object
    /// (named `Miscellaneous`) representing an unit struct.
    ///
    /// **NOTE:** Only the name and description fields can be relied upon when
    /// creating `ApiObject`. Others may be overridden.
    fn unknown_op_object(
        &self,
        path: &str,
        method: HttpMethod,
        op: &Operation<SchemaRepr<Self::Definition>>,
    ) -> Result<ApiObject, Error> {
        let _ = (path, method, op);
        Ok(ApiObject {
            name: "Miscellaneous".into(),
            description: Some(
                "Namespace for operations that cannot be added \
                 to any other modules."
                    .into(),
            ),
            ..Default::default()
        })
    }

    /* MARK: Non-overridable methods */

    /// Entrypoint for emitter. Given an API spec, generate code
    /// inside Rust modules in the configured working directory.
    ///
    /// **NOTE:** Not meant to be overridden.
    fn generate(&self, api: &Api<Self::Definition>) -> Result<(), Error> {
        let state = self.state();
        state.reset_internal_fields();

        // Add default coders.
        let mut coders = api.coders.clone();
        if !coders.contains_key(&JSON_MIME) {
            coders.insert(JSON_MIME.clone(), JSON_CODER.clone());
        }

        if !coders.contains_key(&YAML_MIME) {
            coders.insert(YAML_MIME.clone(), YAML_CODER.clone());
        }

        state.set_media_info(api.spec_format, &coders);

        // Set host and base path.
        if let Some(h) = api.host.as_ref() {
            let mut parts = h.split(':');
            let mut u = state.base_url.borrow_mut();
            if let Some(host) = parts.next() {
                Host::parse(host).map_err(|e| PaperClipError::InvalidHost(h.into(), e))?;
                u.set_host(Some(&host))
                    .expect("expected valid host in URL?");
            }

            if let Some(port) = parts.next() {
                let p = port.parse::<u16>().map_err(|_| {
                    PaperClipError::InvalidHost(h.into(), url::ParseError::InvalidPort)
                })?;
                u.set_port(Some(p)).expect("expected valid port in URL?");
            }
        }

        if let Some(p) = api.base_path.as_ref() {
            state.base_url.borrow_mut().set_path(p);
        }

        let gen = CodegenEmitter(self);
        // Generate file contents by accumulating definitions.
        for (name, schema) in &api.definitions {
            debug!("Creating definition {}", name);
            let schema = schema.read();
            gen.generate_from_definition(&schema)?;
        }

        for (path, map) in &api.paths {
            RequirementCollector {
                path,
                emitter: self,
                api,
                map,
                template_params: HashSet::new(),
            }
            .collect()?;
        }

        state.declare_modules()?;
        state.write_definitions()?;
        state.add_builders()?;
        state.add_client_deps()?;
        state.add_deps()?;

        Ok(())
    }

    /// Builds a schema. This resolves type aliases to known types
    /// and defines/reuses types based on the given context.
    ///
    /// **NOTE:** Not meant to be overridden.
    fn build_def<'a>(
        &self,
        def: &Self::Definition,
        ctx: DefinitionContext<'a>,
    ) -> Result<EmittedUnit, Error> {
        if let Some(ty) = matching_unit_type(def.format(), def.data_type()) {
            trace!("Matches unit type: {}", ty);
            if ctx.define {
                return Ok(EmittedUnit::None);
            }

            return Ok(EmittedUnit::Known(ty.to_owned()));
        }

        match def.data_type() {
            Some(DataType::Array) => CodegenEmitter(self).emit_array(def, ctx),
            Some(DataType::Object) => CodegenEmitter(self).emit_object(def, ctx),
            Some(DataType::File) => {
                // FIXME: Support files.
                warn!("Data type 'file' is unsupported at this point.");
                Ok(EmittedUnit::Known("String".into()))
            }
            Some(_) => unreachable!("bleh?"), // we've already handled everything else
            None => {
                if ctx.define {
                    Ok(EmittedUnit::None)
                } else {
                    Ok(EmittedUnit::Known("String".into()))
                }
            }
        }
    }
}

struct CodegenEmitter<'a, E>(&'a E)
where
    Self: Sized;

impl<'a, E> Deref for CodegenEmitter<'a, E> {
    type Target = E;

    fn deref(&self) -> &E {
        &self.0
    }
}

impl<'a, E> CodegenEmitter<'a, E>
where
    E: Emitter,
    E::Definition: Debug,
{
    /// Given a schema definition, generate the corresponding Rust definition and
    /// add it to `EmitterState`.
    fn generate_from_definition(&self, def: &E::Definition) -> Result<(), Error> {
        // Generate the object.
        let objects = match self.build_def(def, DefinitionContext::default().define(true))? {
            EmittedUnit::Objects(o) => o,
            // We don't care about type aliases because we resolve them anyway.
            _ => return Ok(()),
        };

        self.add_objects_to_path(objects, self.def_mod_path(def)?)
    }

    /// Given a bunch of API objects and their module path, add them to the internal state.
    ///
    /// **NOTE:** Should we need to add any `ApiObject` to `EmitterState.def_mods`, this
    /// method must be used instead of manipulating the field directly.
    fn add_objects_to_path(
        &self,
        mut objects: Vec<ApiObject>,
        mod_path: PathBuf,
    ) -> Result<(), Error> {
        let state = self.state();
        // Create parent dirs recursively for the leaf module.
        let dir_path = mod_path
            .parent()
            .ok_or_else(|| PaperClipError::InvalidDefinitionPath(mod_path.clone()))?;
        if !dir_path.exists() {
            fs::create_dir_all(&dir_path)?;
        }

        // Get the path without the extension.
        let full_path = dir_path.join(
            mod_path
                .file_stem()
                .ok_or_else(|| PaperClipError::InvalidDefinitionPath(mod_path.clone()))?,
        );
        // Get the relative path to the parent.
        let rel_path = full_path
            .strip_prefix(&state.working_dir)
            .map_err(|_| PaperClipError::InvalidDefinitionPath(full_path.clone()))?;

        // Gather the immediate parent-children pairs for module declarations.
        let mut mods = state.mod_children.borrow_mut();
        for (i, path) in rel_path.ancestors().enumerate() {
            if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
                let entry = mods.entry(parent.into()).or_insert_with(HashSet::new);
                entry.insert(ChildModule {
                    name: name.to_string_lossy().into_owned(),
                    is_final: i == 0,
                });
            }
        }

        // Set the relative path to objects for future reference.
        for obj in &mut objects {
            obj.path = rel_path.to_string_lossy().into_owned().replace('/', "::");
        }

        // Add generated object to state.
        let mut def_mods = state.def_mods.borrow_mut();
        def_mods.insert(mod_path, objects);
        Ok(())
    }

    /// Assumes that the given definition is an array and returns the corresponding
    /// vector type for it.
    fn emit_array<'c>(
        &self,
        def: &E::Definition,
        ctx: DefinitionContext<'c>,
    ) -> Result<EmittedUnit, Error> {
        if ctx.define {
            return Ok(EmittedUnit::None);
        }

        let it = def
            .items()
            .ok_or_else(|| PaperClipError::MissingArrayItem(self.def_name(def).ok()))?;

        let schema = it.read();
        let obj = self.build_def(&schema, ctx.define(false))?;
        let ty = String::from("Vec<") + &obj.known_type() + ">";
        Ok(obj.map_known(ty))
    }

    /// Assumes that the given definition is an object and returns the corresponding
    /// Rust struct / map.
    fn emit_object<'c>(
        &self,
        def: &E::Definition,
        ctx: DefinitionContext<'c>,
    ) -> Result<EmittedUnit, Error> {
        match self.try_emit_map(def, &ctx)? {
            EmittedUnit::None => (),
            x => return Ok(x),
        }

        if !ctx.define {
            // If this is an anonymous object, then address it directly.
            if def.name().is_none() {
                let objects = match self.build_def(def, ctx.clone().define(true))? {
                    EmittedUnit::Objects(o) => o,
                    _ => unreachable!(),
                };

                if let Some(name) = self.def_anon_name(def, &ctx.parents) {
                    return Ok(EmittedUnit::KnownButAnonymous(name, objects));
                }
            }

            // Use absolute paths to save some pain.
            let mut ty_path = String::from(self.state().mod_prefix.trim_matches(':'));
            let mut iter = self.def_ns_name(def)?.peekable();
            while let Some(mut c) = iter.next() {
                ty_path.push_str("::");
                if iter.peek().is_none() {
                    ty_path.push_str(&c);
                    ty_path.push_str("::");
                    c = c.to_camel_case();
                }

                ty_path.push_str(&c);
            }

            return Ok(EmittedUnit::Known(ty_path));
        }

        self.emit_struct(def, ctx)
    }

    /// Checks if the given definition is a simple map and returns the corresponding `BTreeMap`.
    fn try_emit_map(
        &self,
        def: &E::Definition,
        ctx: &DefinitionContext<'_>,
    ) -> Result<EmittedUnit, Error> {
        if ctx.define {
            return Ok(EmittedUnit::None);
        }

        if let Some(s) = def.additional_properties() {
            let schema = s.read();
            let ty = self
                .build_def(&schema, ctx.clone().define(false))?
                .known_type();
            let map = format!("std::collections::BTreeMap<String, {}>", ty);
            Ok(EmittedUnit::Known(map))
        } else {
            Ok(EmittedUnit::None)
        }
    }

    /// Helper for `emit_object` - This returns the Rust struct definition for the
    /// given schema definition.
    fn emit_struct(
        &self,
        def: &E::Definition,
        ctx: DefinitionContext<'_>,
    ) -> Result<EmittedUnit, Error> {
        let name = self.def_name(def).or_else(|e| {
            // anonymous object
            self.def_anon_name(def, &ctx.parents).ok_or_else(|| e)
        })?;
        let mut obj = ApiObject::with_name(&name);
        obj.description = def.description().map(String::from);

        // If we don't have any parents and there's a name for this object,
        // then it's the root object - add the name to parents before checking
        // its properties.
        let mut ctx = ctx.clone();
        if ctx.parents.is_empty() && def.name().is_some() {
            ctx = ctx.add_parent(&name);
        }

        // Anonymous objects that we've collected along the way.
        let mut objects = vec![];

        if let Some(props) = def.properties() {
            props
                .iter()
                .try_for_each(|(name, prop)| -> Result<(), Error> {
                    let schema = prop.read();
                    let ctx = ctx.clone().define(false).add_parent(name);
                    let ty = self.build_def(&schema, ctx)?;

                    obj.fields.push(ObjectField {
                        name: name.clone(),
                        description: prop.get_description(),
                        ty_path: ty.known_type(),
                        is_required: def
                            .required_properties()
                            .map(|s| s.contains(name))
                            .unwrap_or(false),
                        boxed: schema.is_cyclic(),
                        child_req_fields: self.children_requirements(&schema),
                    });

                    if let EmittedUnit::KnownButAnonymous(_, mut o) = ty {
                        objects.append(&mut o);
                    }

                    Ok(())
                })?
        }

        objects.insert(0, obj);
        Ok(EmittedUnit::Objects(objects))
    }

    /// Returns the requirements of the "deepest" child type in the given definition.
    ///
    /// See `ObjectField.children_req` field for what it means.
    fn children_requirements(&self, schema: &E::Definition) -> Vec<String> {
        match schema.data_type() {
            Some(DataType::Object) => {
                if let Some(s) = schema.additional_properties() {
                    return self.children_requirements(&s.read());
                } else if let Some(s) = schema.required_properties() {
                    return s.iter().cloned().collect();
                }
            }
            Some(DataType::Array) => {
                if let Some(s) = schema.items() {
                    return self.children_requirements(&s.read());
                }
            }
            _ => (),
        }

        vec![]
    }
}

/// Abstraction which takes care of adding requirements for operations.
struct RequirementCollector<'a, E: Emitter> {
    path: &'a str,
    emitter: &'a E,
    api: &'a Api<E::Definition>,
    map: &'a OperationMap<SchemaRepr<E::Definition>>,
    template_params: HashSet<String>,
}

impl<'a, E> RequirementCollector<'a, E>
where
    E: Emitter,
    E::Definition: Debug,
{
    /// Given a path and an operation map, collect the stuff required
    /// for generating builders later.
    fn collect(mut self) -> Result<(), Error> {
        self.validate_path_and_add_params()?;
        debug!("Collecting builder requirement for {:?}", self.path);

        // Collect all the parameters local to some API call.
        let (unused_params, _) = self.collect_parameters(&self.map.parameters)?;
        // FIXME: What if a body is "required" globally (for all operations)?
        // This means, operations can override the body with some other schema
        // and we may need to map it to the appropriate builders.

        for (&meth, op) in &self.map.methods {
            self.collect_from_operation(meth, op, &unused_params)?;
        }

        // FIXME: If none of the parameters (local to operation or global) specify
        // a body then we should use something (say, `operationID`) to generate
        // a builder and forward `unused_params` to it?
        if self.map.methods.is_empty() {
            warn!(
                "Missing operations for path: {:?}{}",
                self.path,
                if unused_params.is_empty() {
                    ""
                } else {
                    ", but 'parameters' field is specified."
                }
            );
        }

        if !self.template_params.is_empty() {
            return Err(PaperClipError::MissingParametersInPath(
                self.path.into(),
                self.template_params,
            )
            .into());
        }

        Ok(())
    }

    /// Checks whether this path is unique (regardless of its templating)
    /// and returns the list of parameters that exist in the template.
    ///
    /// For example, `/api/{foo}` and `/api/{bar}` are the same, and we
    /// should reject it.
    fn validate_path_and_add_params(&mut self) -> Result<(), PaperClipError> {
        let path_fmt = Api::<()>::path_parameters_map(self.path, |p| {
            self.template_params.insert(p.into());
            ":".into()
        });

        let state = self.emitter.state();
        let mut paths = state.rel_paths.borrow_mut();
        let value_absent = paths.insert(path_fmt.clone().into());
        if value_absent {
            Ok(())
        } else {
            Err(PaperClipError::RelativePathNotUnique(self.path.into()))
        }
    }

    /// Collect the parameters local to an API call operation (method).
    fn collect_from_operation(
        &mut self,
        meth: HttpMethod,
        op: &Operation<SchemaRepr<E::Definition>>,
        unused_params: &[Parameter],
    ) -> Result<(), Error> {
        let (mut params, schema_path) = self.collect_parameters(&op.parameters)?;
        // If we have unused params which don't exist in the method-specific
        // params (which take higher precedence), then we can copy those inside.
        for global_param in unused_params {
            if params
                .iter()
                .find(|p| p.name == global_param.name)
                .is_none()
            {
                params.push(global_param.clone());
            }
        }

        params = params
            .into_iter()
            .filter(|p| {
                let skip = p.presence == ParameterIn::FormData && schema_path.is_some();
                if skip {
                    warn!(
                        "Skipping form data parameter {:?} in path {:?} because \
                         the operation already has a body.",
                        p.name, self.path
                    );
                }

                !skip
            })
            .collect();

        // If there's a matching object, add the params to its operation.
        if let Some(pat) = schema_path.as_ref() {
            self.bind_schema_to_operation(pat, meth, op, params)?;
        } else {
            self.bind_operation_blindly(meth, op, params)?;
        }

        Ok(())
    }

    /// Given a bunch of resolved parameters, validate and collect a simplified version of them.
    fn collect_parameters(
        &mut self,
        obj_params: &[models::Parameter<SchemaRepr<E::Definition>>],
    ) -> Result<(Vec<Parameter>, Option<PathBuf>), Error> {
        let def_mods = self.emitter.state().def_mods.borrow();
        let mut schema_path = None;
        let mut params = vec![];
        for p in obj_params {
            p.check(self.path)?; // validate the parameter

            if let Some(def) = p.schema.as_ref() {
                // If a schema exists, then get its path for later use.
                let pat = self.emitter.def_mod_path(&*def.read())?;
                if def_mods.get(&pat).is_some() {
                    schema_path = Some(pat);
                    continue;
                }

                warn!(
                    "Unregistered schema for parameter {:?} in path {:?}: {:?}",
                    p.name, self.path, def
                );
                continue;
            }

            // If this is a parameter that must exist in path, then remove it
            // from the expected list of parameters.
            if p.in_ == ParameterIn::Path {
                self.template_params.remove(&p.name);
            }

            // Enforce that the parameter is an allowed type and collect it.
            let (ty, mut it_fmts) =
                match resolve_parameter_type(p.data_type, p.format.as_ref(), p.items.as_ref()) {
                    Some(t) => t,
                    None => {
                        warn!(
                            "Skipping parameter {:?} with unknown type {:?} in path {:?}",
                            p.name, p.data_type, self.path
                        );
                        continue;
                    }
                };

            // If it's an array, then validate collection formats and default if needed.
            if p.data_type == Some(DataType::Array) {
                let default_fmt = CollectionFormat::default();
                it_fmts.insert(0, p.collection_format.unwrap_or(default_fmt));
                it_fmts.pop(); // pop the final format, as it's unnecessary.
                let is_url_encoded = p.in_ == ParameterIn::Query || p.in_ == ParameterIn::FormData;
                if it_fmts.contains(&CollectionFormat::Multi) {
                    let needs_override = if is_url_encoded {
                        let mut fmt_idx_iter = it_fmts
                            .iter()
                            .enumerate()
                            .filter(|&(_, &fmt)| fmt == CollectionFormat::Multi);
                        fmt_idx_iter.next().expect("expected collection format?");
                        // We support URL encoding multiple values only when it's specified in root.
                        fmt_idx_iter.next().is_some()
                    } else {
                        true
                    };

                    if needs_override {
                        if is_url_encoded {
                            info!("Parameter {:?} in {:?} doesn't allow multiple instances in nested arrays. \
                                   Replacing with default ({:?}).", p.name, p.in_, default_fmt);
                        } else {
                            info!("Parameter {:?} is in {:?}, which doesn't allow array values as multiple \
                                   instances. Replacing with default ({:?}).", p.name, p.in_, default_fmt);
                        }

                        for (i, f) in it_fmts.iter_mut().enumerate() {
                            if *f == CollectionFormat::Multi {
                                if i == 0 && is_url_encoded {
                                    continue;
                                }

                                *f = default_fmt;
                            }
                        }
                    }
                }
            }

            params.push(Parameter {
                name: p.name.clone(),
                description: p.description.clone(),
                ty_path: ty,
                presence: p.in_,
                // NOTE: parameter is required if it's in path
                required: p.required || p.in_ == ParameterIn::Path,
                delimiting: it_fmts,
            });
        }

        Ok((params, schema_path))
    }

    /// Given a schema path, fetch the object and bind the given operation to it.
    fn bind_schema_to_operation(
        &self,
        schema_path: &Path,
        meth: HttpMethod,
        op: &Operation<SchemaRepr<E::Definition>>,
        params: Vec<Parameter>,
    ) -> Result<(), Error> {
        trace!(
            "Binding {:?} operation in path {:?} to module {:?}",
            meth,
            self.path,
            schema_path
        );

        let state = self.emitter.state();
        let mut def_mods = state.def_mods.borrow_mut();
        let obj = def_mods.get_mut(schema_path).expect("bleh?");
        let ops = obj[0] // first object is always the globally defined object.
            .paths
            .entry(self.path.into())
            .or_insert_with(Default::default);

        ops.req.insert(
            meth,
            OpRequirement {
                listable: false,
                id: op.operation_id.clone(),
                description: op.description.clone(),
                params,
                body_required: true,
                response_ty_path: if let Some(s) = Self::get_2xx_response_schema(&op) {
                    let schema = &*s.read();
                    Some(
                        self.emitter
                            .build_def(schema, DefinitionContext::default())?
                            .known_type(),
                    )
                } else {
                    None
                },
                encoding: self.get_encoder(op),
            },
        );

        Ok(())
    }

    /// We couldn't attach this operation to any object. Now, we're
    /// just attempting out of desperation.
    fn bind_operation_blindly(
        &self,
        meth: HttpMethod,
        op: &Operation<SchemaRepr<E::Definition>>,
        params: Vec<Parameter>,
    ) -> Result<(), Error> {
        // Let's try from the response maybe...
        let s = match Self::get_2xx_response_schema(&op) {
            Some(s) => s,
            None => {
                warn!(
                    "Unable to bind {:?} operation in path {:?} to any known schema.",
                    meth, self.path
                );
                return Ok(());
            }
        };

        let schema = &*s.read();
        let state = self.emitter.state();
        let listable = schema.items().and_then(|s| s.read().data_type()) == Some(DataType::Object);
        let mut unknown_schema_context = None;

        let s = match schema.data_type() {
            // We can deal with object responses.
            Some(DataType::Object) => s.clone(),
            // We can also deal with array of objects by mapping
            // the operation to that object.
            _ if listable => (&**schema.items().unwrap()).clone(),
            // But... we can't deal with simple types or nested arrays, so we
            // let the emitter guess something based on this operation.
            _ => {
                let path = self.emitter.unknown_op_mod_path(self.path, meth, op)?;
                if !state.def_mods.borrow().contains_key(&path) {
                    // NOTE: Don't add `ApiObject` directly, because we have to
                    // set appropriate paths.
                    CodegenEmitter(self.emitter).add_objects_to_path(
                        vec![self.emitter.unknown_op_object(self.path, meth, op)?],
                        path.clone(),
                    )?;
                }

                unknown_schema_context = Some((
                    path,
                    self.emitter
                        .build_def(schema, DefinitionContext::default())?
                        .known_type(),
                ));
                s.clone()
            }
        };

        let schema = &*s.read();
        let mut def_mods = state.def_mods.borrow_mut();
        let (obj, response_ty_path) = match unknown_schema_context {
            Some((p, ty)) => (
                def_mods.get_mut(&p).expect("expected misc API object"),
                Some(ty),
            ),
            // If this is known, then we should be able to get the object.
            None => match self
                .emitter
                .def_mod_path(schema)
                .ok()
                .and_then(|p| def_mods.get_mut(&p))
            {
                Some(o) => (o, None),
                None => {
                    warn!(
                        "Skipping unknown response schema for path {:?}: {:?}",
                        self.path, schema
                    );
                    return Ok(());
                }
            },
        };

        let ops = obj[0] // first object is always the globally defined object.
            .paths
            .entry(self.path.into())
            .or_insert_with(Default::default);

        ops.req.insert(
            meth,
            OpRequirement {
                id: op.operation_id.clone(),
                description: op.description.clone(),
                params,
                body_required: false,
                listable,
                response_ty_path,
                encoding: self.get_encoder(op),
            },
        );

        Ok(())
    }

    /// Returns the first 2xx response schema in this operation.
    ///
    /// **NOTE:** This assumes that 2xx response schemas are the same for an operation.
    fn get_2xx_response_schema<'o>(
        op: &'o Operation<SchemaRepr<E::Definition>>,
    ) -> Option<&'o ArcRwLock<E::Definition>> {
        op.responses
            .iter()
            .filter(|(c, _)| c.starts_with('2')) // 2xx response
            .filter_map(|(_, r)| r.schema.as_ref())
            .next()
            .map(|r| &**r)
    }

    /// Returns the encoder for the given operation (if any, if required).
    /// Returns `None` if it's JSON (since we already support it).
    fn get_encoder(
        &self,
        op: &Operation<SchemaRepr<E::Definition>>,
    ) -> Option<(String, Arc<Coder>)> {
        let consumes = match op.consumes.as_ref() {
            Some(s) => s,
            None => &self.api.consumes,
        };

        let mut encoders = consumes
            .iter()
            .filter_map(|r| self.api.coders.matching_coder(r).map(|c| (r, c)))
            .sorted_by(|(_, a), (_, b)| b.prefer.cmp(&a.prefer)); // sort based on preference.

        let (range, coder) = encoders
            .next()
            .unwrap_or_else(|| (self.api.spec_format.mime(), self.api.spec_format.coder()));
        if range == &*JSON_MIME {
            return None;
        }

        Some((range.0.as_ref().into(), coder))
    }
}

/// Ensures that a parameter type is either a simple type or an array
/// and returns the resolved Rust type.
fn resolve_parameter_type(
    dt: Option<DataType>,
    dt_fmt: Option<&DataTypeFormat>,
    items: Option<&Items>,
) -> Option<(String, Vec<CollectionFormat>)> {
    match matching_unit_type(dt_fmt, dt) {
        Some(t) => return Some((t.into(), vec![])),
        None if dt == Some(DataType::Array) => {
            if let Some(i) = items {
                if let Some((ty, mut fmts)) = resolve_parameter_type(
                    i.data_type,
                    i.format.as_ref(),
                    i.items.as_ref().map(Deref::deref),
                ) {
                    fmts.insert(0, i.collection_format.unwrap_or_default());
                    // We collect it as `Vec` for now - we'll replace it with our
                    // `Delimited` wrapper when we actually write the code.
                    return Some((String::from("Vec<") + ty.as_str() + ">", fmts));
                }
            }
        }
        None => (),
    }

    None
}

/// Checks if the given type/format matches a known Rust type and returns it.
fn matching_unit_type(
    format: Option<&DataTypeFormat>,
    type_: Option<DataType>,
) -> Option<&'static str> {
    match format {
        Some(DataTypeFormat::Int32) => Some("i32"),
        Some(DataTypeFormat::Int64) => Some("i64"),
        Some(DataTypeFormat::Float) => Some("f32"),
        Some(DataTypeFormat::Double) => Some("f64"),
        _ => match type_ {
            Some(DataType::Integer) => Some("i64"),
            Some(DataType::Number) => Some("f64"),
            Some(DataType::Boolean) => Some("bool"),
            Some(DataType::String) => Some("String"),
            _ => None,
        },
    }
}
