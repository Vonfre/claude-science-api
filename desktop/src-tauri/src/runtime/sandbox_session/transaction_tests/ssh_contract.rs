#[test]
fn ssh_wrapper_prevalidation_uses_the_running_runtime_validator_before_oauth() {
    use syn::visit::{self, Visit};
    use syn::{
        Attribute, Expr, ExprCall, ExprLit, GenericArgument, Item, ItemFn, Lit, Pat, PathArguments,
        Stmt, Type,
    };

    fn top_level<'a>(file: &'a syn::File, name: &str) -> &'a ItemFn {
        file.items
            .iter()
            .find_map(|item| match item {
                Item::Fn(function) if function.sig.ident == name => Some(function),
                _ => None,
            })
            .unwrap_or_else(|| panic!("missing module-level product function {name}"))
    }

    fn is_cfg(attribute: &Attribute) -> bool {
        attribute.path().is_ident("cfg") || attribute.path().is_ident("cfg_attr")
    }

    fn cfg_tokens(attribute: &Attribute) -> String {
        attribute
            .meta
            .require_list()
            .map(|list| list.tokens.to_string())
            .unwrap_or_default()
    }

    fn reject_cfg(attributes: &[Attribute], label: &str) {
        assert!(
            !attributes.iter().any(is_cfg),
            "{label} must be present in every release build"
        );
    }

    #[derive(Default)]
    struct Facts {
        calls: Vec<String>,
        strings: Vec<String>,
        has_cfg: bool,
    }

    impl<'ast> Visit<'ast> for Facts {
        fn visit_attribute(&mut self, attribute: &'ast Attribute) {
            self.has_cfg |= is_cfg(attribute);
            visit::visit_attribute(self, attribute);
        }

        fn visit_expr_call(&mut self, call: &'ast ExprCall) {
            if let Expr::Path(path) = &*call.func {
                if let Some(segment) = path.path.segments.last() {
                    self.calls.push(segment.ident.to_string());
                }
            }
            visit::visit_expr_call(self, call);
        }

        fn visit_expr_lit(&mut self, literal: &'ast ExprLit) {
            if let Lit::Str(value) = &literal.lit {
                self.strings.push(value.value());
            }
            visit::visit_expr_lit(self, literal);
        }

        fn visit_item_fn(&mut self, _function: &'ast ItemFn) {}
        fn visit_expr_closure(&mut self, _closure: &'ast syn::ExprClosure) {}
        fn visit_expr_async(&mut self, _expression: &'ast syn::ExprAsync) {}
    }

    fn statement_facts(statement: &Stmt) -> Facts {
        let mut facts = Facts::default();
        facts.visit_stmt(statement);
        facts
    }

    fn function_facts(function: &ItemFn) -> Facts {
        let mut facts = Facts::default();
        facts.visit_block(&function.block);
        facts
    }

    fn local_name(local: &syn::Local) -> Option<&syn::Ident> {
        match &local.pat {
            Pat::Ident(ident) => Some(&ident.ident),
            Pat::Type(typed) => match &*typed.pat {
                Pat::Ident(ident) => Some(&ident.ident),
                _ => None,
            },
            _ => None,
        }
    }

    fn peel_expression(mut expression: &Expr) -> &Expr {
        loop {
            expression = match expression {
                Expr::Group(group) => &group.expr,
                Expr::Paren(paren) => &paren.expr,
                _ => return expression,
            };
        }
    }

    fn direct_zero_arg_closure_body(local: &syn::Local) -> Option<&syn::Block> {
        let initializer = local.init.as_ref()?;
        let Expr::Call(call) = peel_expression(&initializer.expr) else {
            return None;
        };
        if !call.args.is_empty() {
            return None;
        }
        let Expr::Closure(closure) = peel_expression(&call.func) else {
            return None;
        };
        closure
            .inputs
            .is_empty()
            .then_some(&closure.body)
            .and_then(|body| match peel_expression(body) {
                Expr::Block(block) => Some(&block.block),
                _ => None,
            })
    }

    fn direct_call(expression: &Expr) -> Option<&ExprCall> {
        match expression {
            Expr::Call(call) => Some(call),
            Expr::Await(awaited) => direct_call(&awaited.base),
            Expr::Group(group) => direct_call(&group.expr),
            Expr::Paren(paren) => direct_call(&paren.expr),
            Expr::Try(tried) => direct_call(&tried.expr),
            // Typed failure projection wraps Result edges as `.map_err(...)?`.
            Expr::MethodCall(method)
                if method.method == "map_err" || method.method == "map_err_kind" =>
            {
                direct_call(&method.receiver)
            }
            _ => None,
        }
    }

    fn call_path(call: &ExprCall) -> Option<String> {
        let Expr::Path(path) = &*call.func else {
            return None;
        };
        Some(
            path.path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        )
    }

    fn expression_path(expression: &Expr) -> Option<String> {
        let Expr::Path(path) = expression else {
            return None;
        };
        Some(
            path.path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        )
    }

    fn statement_directly_calls(statement: &Stmt, expected: &str) -> bool {
        let expression = match statement {
            Stmt::Local(local) => local.init.as_ref().map(|init| &*init.expr),
            Stmt::Expr(expression, _) => Some(expression),
            _ => None,
        };
        expression
            .and_then(direct_call)
            .and_then(call_path)
            .as_deref()
            == Some(expected)
    }

    fn simple_argument_name(expression: &Expr) -> Option<String> {
        let expression = match expression {
            Expr::Reference(reference) => &*reference.expr,
            Expr::Group(group) => &*group.expr,
            Expr::Paren(paren) => &*paren.expr,
            expression => expression,
        };
        let Expr::Path(path) = expression else {
            return None;
        };
        (path.qself.is_none() && path.path.segments.len() == 1)
            .then(|| path.path.segments[0].ident.to_string())
    }

    fn statement_direct_call_arguments(statement: &Stmt) -> Option<Vec<String>> {
        let expression = match statement {
            Stmt::Local(local) => local.init.as_ref().map(|init| &*init.expr),
            Stmt::Expr(expression, _) => Some(expression),
            _ => None,
        }?;
        let call = direct_call(expression)?;
        call.args.iter().map(simple_argument_name).collect()
    }

    fn statement_propagates_direct_call(statement: &Stmt, expected: &str) -> bool {
        let expression = match statement {
            Stmt::Local(local) => local.init.as_ref().map(|init| &*init.expr),
            Stmt::Expr(expression, _) => Some(expression),
            _ => None,
        };
        let Some(Expr::Try(tried)) = expression else {
            return false;
        };
        // Accept both `call?` and `call.map_err(...)?` (typed failure projection).
        let call = match &*tried.expr {
            Expr::Call(call) => call,
            Expr::MethodCall(method)
                if (method.method == "map_err" || method.method == "map_err_kind") =>
            {
                match &*method.receiver {
                    Expr::Call(call) => call,
                    _ => return false,
                }
            }
            _ => return false,
        };
        call_path(call).as_deref() == Some(expected)
    }

    fn returns_result_pathbuf_string(function: &ItemFn) -> bool {
        let syn::ReturnType::Type(_, returned) = &function.sig.output else {
            return false;
        };
        let Type::Path(path) = &**returned else {
            return false;
        };
        let Some(result) = path.path.segments.last() else {
            return false;
        };
        if result.ident != "Result" {
            return false;
        }
        let PathArguments::AngleBracketed(arguments) = &result.arguments else {
            return false;
        };
        let types = arguments
            .args
            .iter()
            .filter_map(|argument| match argument {
                GenericArgument::Type(Type::Path(path)) => path
                    .path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>();
        types == ["PathBuf", "String"]
    }

    #[derive(Default)]
    struct EarlyExitFacts {
        count: usize,
    }

    impl<'ast> Visit<'ast> for EarlyExitFacts {
        fn visit_expr_return(&mut self, expression: &'ast syn::ExprReturn) {
            self.count += 1;
            visit::visit_expr_return(self, expression);
        }

        fn visit_expr_break(&mut self, expression: &'ast syn::ExprBreak) {
            self.count += 1;
            visit::visit_expr_break(self, expression);
        }

        fn visit_expr_continue(&mut self, expression: &'ast syn::ExprContinue) {
            self.count += 1;
            visit::visit_expr_continue(self, expression);
        }

        fn visit_expr_loop(&mut self, expression: &'ast syn::ExprLoop) {
            self.count += 1;
            visit::visit_expr_loop(self, expression);
        }

        fn visit_expr_while(&mut self, expression: &'ast syn::ExprWhile) {
            self.count += 1;
            visit::visit_expr_while(self, expression);
        }

        fn visit_expr_for_loop(&mut self, expression: &'ast syn::ExprForLoop) {
            self.count += 1;
            visit::visit_expr_for_loop(self, expression);
        }

        fn visit_expr_call(&mut self, call: &'ast ExprCall) {
            if call_path(call).is_some_and(|path| {
                matches!(
                    path.rsplit("::").next(),
                    Some("exit" | "abort" | "abort_internal")
                )
            }) {
                self.count += 1;
            }
            visit::visit_expr_call(self, call);
        }

        fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
            if invocation.path.segments.last().is_some_and(|segment| {
                matches!(
                    segment.ident.to_string().as_str(),
                    "panic" | "todo" | "unreachable"
                )
            }) || token_stream_contains_any(
                invocation.tokens.clone(),
                &["return", "break", "continue"],
            ) {
                self.count += 1;
            }
            visit::visit_macro(self, invocation);
        }
    }

    #[derive(Default)]
    struct ProductLiterals(Vec<String>);

    impl<'ast> Visit<'ast> for ProductLiterals {
        fn visit_expr_lit(&mut self, literal: &'ast ExprLit) {
            if let Lit::Str(value) = &literal.lit {
                self.0.push(value.value());
            }
            visit::visit_expr_lit(self, literal);
        }
    }

    fn use_tree_contains_ident(tree: &syn::UseTree, expected: &str) -> bool {
        match tree {
            syn::UseTree::Path(path) => {
                path.ident == expected || use_tree_contains_ident(&path.tree, expected)
            }
            syn::UseTree::Name(name) => name.ident == expected,
            syn::UseTree::Rename(rename) => rename.ident == expected,
            syn::UseTree::Group(group) => group
                .items
                .iter()
                .any(|tree| use_tree_contains_ident(tree, expected)),
            syn::UseTree::Glob(_) => false,
        }
    }

    fn token_stream_contains_any(tokens: proc_macro2::TokenStream, expected: &[&str]) -> bool {
        tokens.into_iter().any(|token| match token {
            proc_macro2::TokenTree::Ident(ident) => expected.iter().any(|value| ident == *value),
            proc_macro2::TokenTree::Group(group) => {
                token_stream_contains_any(group.stream(), expected)
            }
            _ => false,
        })
    }

    #[derive(Default)]
    struct ForbiddenCfgMacros(usize);

    impl<'ast> Visit<'ast> for ForbiddenCfgMacros {
        fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
            if invocation
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "cfg")
                || token_stream_contains_any(invocation.tokens.clone(), &["cfg"])
            {
                self.0 += 1;
            }
            visit::visit_macro(self, invocation);
        }

        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            if use_tree_contains_ident(&item.tree, "cfg") {
                self.0 += 1;
            }
            visit::visit_item_use(self, item);
        }
    }

    #[derive(Default)]
    struct ValidatorFacts {
        cfg_attributes: Vec<String>,
        environment_reads: Vec<String>,
        environment_paths: Vec<String>,
        environment_imports: usize,
    }

    #[derive(Default)]
    struct ProductEnvironmentFacts {
        environment_paths: Vec<String>,
        environment_imports: usize,
    }

    impl<'ast> Visit<'ast> for ProductEnvironmentFacts {
        fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
            let path = expression
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            let last = path.rsplit("::").next().unwrap_or_default();
            if path.split("::").any(|segment| segment == "env")
                || matches!(last, "var" | "var_os" | "vars" | "vars_os")
            {
                self.environment_paths.push(path);
            }
            visit::visit_expr_path(self, expression);
        }

        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            if use_tree_contains_ident(&item.tree, "env") {
                self.environment_imports += 1;
            }
            visit::visit_item_use(self, item);
        }

        fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
            if token_stream_contains_any(
                invocation.tokens.clone(),
                &["env", "var", "var_os", "vars", "vars_os"],
            ) {
                self.environment_imports += 1;
            }
            visit::visit_macro(self, invocation);
        }
    }

    impl<'ast> Visit<'ast> for ValidatorFacts {
        fn visit_attribute(&mut self, attribute: &'ast Attribute) {
            if is_cfg(attribute) {
                self.cfg_attributes.push(cfg_tokens(attribute));
            }
            visit::visit_attribute(self, attribute);
        }

        fn visit_expr_call(&mut self, call: &'ast ExprCall) {
            if let Some(path) = call_path(call) {
                let last = path.rsplit("::").next().unwrap_or_default();
                if path.split("::").any(|segment| segment == "env")
                    || matches!(last, "var" | "var_os" | "vars" | "vars_os")
                {
                    self.environment_reads.push(path);
                }
            }
            visit::visit_expr_call(self, call);
        }

        fn visit_expr_path(&mut self, expression: &'ast syn::ExprPath) {
            let path = expression
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::");
            let last = path.rsplit("::").next().unwrap_or_default();
            if path.split("::").any(|segment| segment == "env")
                || matches!(last, "var" | "var_os" | "vars" | "vars_os")
            {
                self.environment_paths.push(path);
            }
            visit::visit_expr_path(self, expression);
        }

        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            if use_tree_contains_ident(&item.tree, "env") {
                self.environment_imports += 1;
            }
            visit::visit_item_use(self, item);
        }
    }

    #[derive(Default)]
    struct WrapperLocalCount(usize);

    impl<'ast> Visit<'ast> for WrapperLocalCount {
        fn visit_local(&mut self, local: &'ast syn::Local) {
            if local_name(local).is_some_and(|name| name == "wrapper_override") {
                self.0 += 1;
            }
            visit::visit_local(self, local);
        }
    }

    #[derive(Default)]
    struct CfgAttributes(Vec<String>);

    impl<'ast> Visit<'ast> for CfgAttributes {
        fn visit_attribute(&mut self, attribute: &'ast Attribute) {
            if is_cfg(attribute) {
                self.0.push(cfg_tokens(attribute));
            }
            visit::visit_attribute(self, attribute);
        }
    }

    fn exact_test_override(local: &syn::Local) -> bool {
        if local.attrs.len() != 1
            || !local.attrs[0].path().is_ident("cfg")
            || cfg_tokens(&local.attrs[0]) != "test"
            || !matches!(&local.pat, Pat::Ident(ident) if ident.ident == "wrapper_override")
        {
            return false;
        }
        let Some(initializer) = &local.init else {
            return false;
        };
        let Expr::MethodCall(mapped) = &*initializer.expr else {
            return false;
        };
        if mapped.method != "map" || mapped.args.len() != 1 {
            return false;
        }
        let Expr::Call(read) = &*mapped.receiver else {
            return false;
        };
        if call_path(read).as_deref() != Some("std::env::var_os") || read.args.len() != 1 {
            return false;
        }
        if !matches!(
            read.args.first(),
            Some(Expr::Lit(ExprLit {
                lit: Lit::Str(value),
                ..
            })) if value.value() == "CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE"
        ) {
            return false;
        }
        expression_path(mapped.args.first().unwrap()).as_deref() == Some("PathBuf::from")
    }

    fn option_pathbuf_type(pattern: &Pat) -> bool {
        let Pat::Type(typed) = pattern else {
            return false;
        };
        if !matches!(&*typed.pat, Pat::Ident(ident) if ident.ident == "wrapper_override") {
            return false;
        }
        let Type::Path(path) = &*typed.ty else {
            return false;
        };
        let Some(option) = path.path.segments.last() else {
            return false;
        };
        if option.ident != "Option" {
            return false;
        }
        let PathArguments::AngleBracketed(arguments) = &option.arguments else {
            return false;
        };
        matches!(
            arguments.args.first(),
            Some(GenericArgument::Type(Type::Path(inner)))
                if inner.path.segments.last().is_some_and(|segment| segment.ident == "PathBuf")
        )
    }

    fn exact_release_override(local: &syn::Local) -> bool {
        if local.attrs.len() != 1
            || !local.attrs[0].path().is_ident("cfg")
            || cfg_tokens(&local.attrs[0]).replace(' ', "") != "not(test)"
            || !option_pathbuf_type(&local.pat)
        {
            return false;
        }
        let Some(initializer) = &local.init else {
            return false;
        };
        expression_path(&initializer.expr).as_deref() == Some("None")
    }

    // SSH validators live in ssh_preflight; entry dispatch and cold orchestration stay separate.
    let one_click_file = syn::parse_file(include_str!("../one_click.rs"))
        .expect("one-click product Rust source must parse");
    let cold_file = syn::parse_file(include_str!("../one_click/cold.rs"))
        .expect("cold one-click Rust source must parse");
    let compensation_file = syn::parse_file(include_str!("../one_click/cold/compensation.rs"))
        .expect("one-click compensation phase Rust source must parse");
    let science_phase_file = syn::parse_file(include_str!("../one_click/cold/science_phase.rs"))
        .expect("managed Science phase Rust source must parse");
    let ssh = syn::parse_file(include_str!("../ssh_preflight.rs"))
        .expect("ssh_preflight product Rust source must parse");
    let mut forbidden_cfg_macros = ForbiddenCfgMacros::default();
    forbidden_cfg_macros.visit_file(&one_click_file);
    forbidden_cfg_macros.visit_file(&cold_file);
    forbidden_cfg_macros.visit_file(&compensation_file);
    forbidden_cfg_macros.visit_file(&science_phase_file);
    forbidden_cfg_macros.visit_file(&ssh);
    assert_eq!(
        forbidden_cfg_macros.0, 0,
        "product SSH transaction source must not branch on cfg!(test)"
    );
    let validator = top_level(&ssh, "validate_system_ssh_wrapper_path");
    let running = top_level(&ssh, "validate_running_system_ssh_bridge");
    let prevalidation = top_level(&ssh, "prevalidate_one_click_system_ssh");
    let one_click = top_level(&one_click_file, "one_click_login_with_options");
    let cold = top_level(&cold_file, "run_cold_one_click");
    let compensation = top_level(&compensation_file, "compensate_one_click_failure");
    let science_phase = top_level(&science_phase_file, "run_managed_science_launch_phase");
    assert!(
        returns_result_pathbuf_string(validator),
        "shared wrapper validator must return Result<PathBuf, String>"
    );
    let mut product_environment = ProductEnvironmentFacts::default();
    product_environment.visit_file(&one_click_file);
    product_environment.visit_file(&cold_file);
    product_environment.visit_file(&compensation_file);
    product_environment.visit_file(&science_phase_file);
    product_environment.visit_file(&ssh);
    product_environment.environment_paths.sort();
    assert_eq!(
        product_environment.environment_imports, 0,
        "product SSH transaction source must not import or alias environment APIs"
    );
    assert_eq!(
            product_environment.environment_paths,
            [
                "std::env::var".to_string(),
                "std::env::var".to_string(),
                "std::env::var_os".to_string(),
                "std::env::var_os".to_string(),
                "std::env::var_os".to_string(),
            ],
            "product transaction source may reference only the DB reverify/restart-budget seams, exact wrapper override, host-proof seam, and exact late-failure seam environment APIs"
        );

    for (name, function) in [
        ("shared wrapper validator", validator),
        ("running SSH validator", running),
        ("pre-OAuth SSH validator", prevalidation),
        ("one-click entry path", one_click),
        ("cold one-click product path", cold),
        ("one-click compensation phase", compensation),
        ("managed Science launch phase", science_phase),
    ] {
        reject_cfg(&function.attrs, name);
    }

    {
        let (name, function) = ("running SSH validator", running);
        let facts = function_facts(function);
        assert!(
            !facts.has_cfg,
            "{name} must not contain cfg-gated call sites"
        );
    }
    let prevalidation_facts = function_facts(prevalidation);
    assert!(
        !prevalidation_facts.has_cfg,
        "pre-OAuth SSH validation must not contain cfg-gated call sites"
    );
    assert_eq!(
        prevalidation_facts
            .calls
            .iter()
            .filter(|call| *call == "validate_system_ssh_wrapper_path")
            .count(),
        1,
        "enabled pre-OAuth validation must use the same shared wrapper validator exactly once"
    );
    for required in [
        "prevalidate_science_ssh_bridge",
        "prevalidate_sandbox_ssh_stub",
    ] {
        assert!(
                prevalidation_facts.calls.iter().any(|call| call == required),
                "pre-OAuth validation must preserve disabled-mode read-only conflict validation via {required}"
            );
    }

    {
        let (name, function) = ("running SSH validator", running);
        let shared_call_positions = function
            .block
            .stmts
            .iter()
            .enumerate()
            .filter_map(|(index, statement)| {
                statement_directly_calls(
                    statement,
                    "crate::runtime::sandbox_session::validate_system_ssh_wrapper_path",
                )
                .then_some(index)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            shared_call_positions,
            [0],
            "{name} must execute the shared wrapper validator exactly once as its first statement"
        );
        assert_eq!(
            statement_direct_call_arguments(&function.block.stmts[0]),
            Some(vec!["app".to_string()]),
            "{name} shared-validator call may receive only the simple app argument"
        );
        assert!(
            statement_propagates_direct_call(
                &function.block.stmts[0],
                "crate::runtime::sandbox_session::validate_system_ssh_wrapper_path",
            ),
            "{name} must propagate the shared-validator Result with an exact Try(Call)"
        );
        let mut call_exit = EarlyExitFacts::default();
        call_exit.visit_stmt(&function.block.stmts[0]);
        assert_eq!(
                call_exit.count, 0,
                "{name} shared-validator call statement must not hide early-exit control flow in its arguments"
            );
    }

    let prevalidate_statement = cold
        .block
        .stmts
        .iter()
        .position(|statement| {
            matches!(statement, Stmt::Local(_))
                && statement_directly_calls(
                    statement,
                    "crate::runtime::sandbox_session::prevalidate_one_click_system_ssh",
                )
        })
        .expect("one-click must execute prevalidation in a top-level local statement");
    assert_eq!(
        cold.block
            .stmts
            .iter()
            .filter(|statement| {
                statement_directly_calls(
                    statement,
                    "crate::runtime::sandbox_session::prevalidate_one_click_system_ssh",
                )
            })
            .count(),
        1,
        "one-click must execute exactly one direct prevalidation call"
    );
    assert_eq!(
        statement_direct_call_arguments(&cold.block.stmts[prevalidate_statement]),
        Some(vec![
            "app".to_string(),
            "cfg".to_string(),
            "sbx_home".to_string(),
        ]),
        "one-click prevalidation may receive only simple app, cfg, and sbx_home arguments"
    );
    assert!(
        statement_propagates_direct_call(
            &cold.block.stmts[prevalidate_statement],
            "crate::runtime::sandbox_session::prevalidate_one_click_system_ssh",
        ),
        "one-click must propagate the prevalidation Result with an exact Try(Call)"
    );
    let entry_facts_statement = one_click
        .block
        .stmts
        .iter()
        .position(|statement| {
            statement_facts(statement)
                .calls
                .iter()
                .any(|call| call == "capture_one_click_entry_facts")
        })
        .expect("one-click must capture immutable entry facts");
    let entry_decision_statement = one_click
        .block
        .stmts
        .iter()
        .position(|statement| {
            statement_facts(statement)
                .calls
                .iter()
                .any(|call| call == "decide_one_click_entry")
        })
        .expect("one-click must decide its route from immutable facts");
    assert!(
        entry_facts_statement < entry_decision_statement
            && !function_facts(one_click)
                .calls
                .iter()
                .any(|call| call == "prevalidate_one_click_system_ssh")
            && function_facts(one_click)
                .calls
                .iter()
                .any(|call| call == "run_cold_one_click"),
        "one-click must decide the route before delegating all cold SSH effects"
    );
    let authority_transaction_statement = cold
        .block
        .stmts
        .iter()
        .position(|statement| {
            matches!(
                statement,
                Stmt::Local(local)
                    if local_name(local).is_some_and(|name| name == "authority_transaction")
            )
        })
        .expect("one-click authority transaction statement must exist");
    let transaction_statement = cold
        .block
        .stmts
        .iter()
        .position(|statement| {
            matches!(
                statement,
                Stmt::Local(local)
                    if local_name(local).is_some_and(|name| name == "transaction_result")
            )
        })
        .expect("one-click transaction_result statement must exist");
    assert!(
        prevalidate_statement < authority_transaction_statement
            && authority_transaction_statement < transaction_statement,
        "SSH prevalidation must precede authority transaction capture and the mutation transaction"
    );
    assert!(
        cold.block.stmts[..transaction_statement]
            .iter()
            .all(|statement| !statement_facts(statement)
                .calls
                .iter()
                .any(|call| call == "ensure_virtual_login")),
        "one-click must not execute OAuth mutation before transaction_result"
    );
    let transaction_local = match &cold.block.stmts[transaction_statement] {
        Stmt::Local(local) => local,
        _ => unreachable!(),
    };
    let transaction_body = direct_zero_arg_closure_body(transaction_local)
        .expect("transaction_result must directly invoke one zero-argument closure block");
    let oauth_statements = transaction_body
        .stmts
        .iter()
        .enumerate()
        .filter(|(_, statement)| {
            statement_facts(statement)
                .calls
                .iter()
                .any(|call| call == "ensure_virtual_login")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        oauth_statements.len(),
        1,
        "transaction body must contain exactly one top-level OAuth mutation statement"
    );
    let mut one_click_cfg = CfgAttributes::default();
    one_click_cfg.visit_block(&cold.block);
    one_click_cfg.visit_block(&science_phase.block);
    assert_eq!(
        one_click_cfg.0,
        ["test".to_string(), "test".to_string()],
        "one-click may contain only the exact host-proof and late-failure cfg(test) seams"
    );
    let phase_dispatch_statements = transaction_body
        .stmts
        .iter()
        .enumerate()
        .filter(|(_, statement)| {
            statement_facts(statement)
                .calls
                .iter()
                .any(|call| call == "run_managed_science_launch_phase")
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    assert_eq!(
        phase_dispatch_statements.len(),
        1,
        "transaction body must contain exactly one managed Science phase dispatch"
    );
    assert!(
        oauth_statements[0] < phase_dispatch_statements[0],
        "managed Science launch must remain after OAuth mutation"
    );
    let late_seam_statements = science_phase
        .block
        .stmts
        .iter()
        .filter(|statement| {
            let facts = statement_facts(statement);
            facts.has_cfg
                && facts
                    .strings
                    .iter()
                    .any(|value| value == "CSSWITCH_TEST_SSH_LATE_FOREIGN_STUB")
                && matches!(
                    statement,
                    Stmt::Expr(Expr::If(expression), _)
                        if expression.attrs.len() == 1
                            && expression.attrs[0].path().is_ident("cfg")
                            && cfg_tokens(&expression.attrs[0]) == "test"
                )
        })
        .count();
    assert_eq!(
        late_seam_statements, 1,
        "managed Science phase must contain exactly one top-level exact cfg(test) late-failure seam"
    );

    let wrapper_locals = validator
        .block
        .stmts
        .iter()
        .filter_map(|statement| match statement {
            Stmt::Local(local)
                if local_name(local).is_some_and(|name| name == "wrapper_override") =>
            {
                Some(local)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut wrapper_local_count = WrapperLocalCount::default();
    wrapper_local_count.visit_block(&validator.block);
    assert_eq!(
        wrapper_local_count.0, 2,
        "shared validator must not hide extra wrapper_override locals in nested product code"
    );
    assert_eq!(
        wrapper_locals.len(),
        2,
        "shared validator must define test and release wrapper_override locals"
    );
    assert!(
            wrapper_locals.iter().any(|local| exact_test_override(local)),
            "test wrapper_override must be the sole cfg(test) var_os literal mapped through PathBuf::from"
        );
    assert!(
        wrapper_locals
            .iter()
            .any(|local| exact_release_override(local)),
        "release wrapper_override must be exactly cfg(not(test)) Option<PathBuf> = None"
    );
    let mut validator_facts = ValidatorFacts::default();
    validator_facts.visit_block(&validator.block);
    validator_facts.cfg_attributes.sort();
    assert_eq!(
        validator_facts.cfg_attributes,
        ["not (test)".to_string(), "test".to_string()],
        "shared validator may contain only the two exact wrapper_override cfg attributes"
    );
    assert_eq!(
        validator_facts.environment_reads,
        ["std::env::var_os".to_string()],
        "shared validator may perform only the guarded test var_os environment read"
    );
    assert_eq!(
        validator_facts.environment_paths,
        ["std::env::var_os".to_string()],
        "shared validator may reference only the guarded test var_os environment path"
    );
    assert_eq!(
        validator_facts.environment_imports, 0,
        "shared validator must not import or alias environment APIs"
    );
    let mut product_literals = ProductLiterals::default();
    product_literals.visit_file(&one_click_file);
    product_literals.visit_file(&ssh);
    assert_eq!(
        product_literals
            .0
            .iter()
            .filter(|value| value.as_str() == "CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE")
            .count(),
        1,
        "the test wrapper environment variable may appear only in its guarded local"
    );

    struct RestoreWrapperOverride(Option<std::ffi::OsString>);
    impl Drop for RestoreWrapperOverride {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE", value),
                None => std::env::remove_var("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE"),
            }
        }
    }

    use std::os::unix::fs::PermissionsExt;
    let _override_guard =
        RestoreWrapperOverride(std::env::var_os("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE"));
    let root = std::env::temp_dir().join(format!(
        "csswitch-shared-ssh-validator-{}-{}",
        std::process::id(),
        crate::config::new_id()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let missing = root.join("missing-wrapper");
    std::env::set_var("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE", &missing);
    let app = tauri::test::mock_builder()
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .unwrap();
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap_err(),
        "打包的 CSSwitch SSH bridge 缺失"
    );
    let wrapper = root.join("ssh");
    std::fs::write(
        &wrapper,
        include_bytes!("../../../../../../scripts/ssh-bridge/ssh"),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::env::set_var("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE", &wrapper);
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap_err(),
        "打包的 CSSwitch SSH bridge 不是安全的可执行文件"
    );
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap(),
        wrapper
    );
    let mut wrong_content = include_bytes!("../../../../../../scripts/ssh-bridge/ssh").to_vec();
    wrong_content[0] = b'x';
    std::fs::write(&wrapper, wrong_content).unwrap();
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap_err(),
        "打包的 CSSwitch SSH bridge 内容身份不匹配"
    );
    std::fs::write(
        &wrapper,
        include_bytes!("../../../../../../scripts/ssh-bridge/ssh"),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o722)).unwrap();
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap_err(),
        "打包的 CSSwitch SSH bridge 不是安全的可执行文件"
    );
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let wrapper_hardlink = root.join("ssh-hardlink");
    std::fs::hard_link(&wrapper, &wrapper_hardlink).unwrap();
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap_err(),
        "打包的 CSSwitch SSH bridge 不是安全的可执行文件"
    );
    std::fs::remove_file(wrapper_hardlink).unwrap();
    let wrapper_link = root.join("ssh-link");
    std::os::unix::fs::symlink(&wrapper, &wrapper_link).unwrap();
    std::env::set_var("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE", &wrapper_link);
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap_err(),
        "打包的 CSSwitch SSH bridge 不是安全的可执行文件"
    );
    let wrapper_directory = root.join("ssh-directory");
    std::fs::create_dir(&wrapper_directory).unwrap();
    std::fs::set_permissions(&wrapper_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::env::set_var("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE", &wrapper_directory);
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap_err(),
        "打包的 CSSwitch SSH bridge 不是安全的可执行文件"
    );
    let oversized_wrapper = root.join("ssh-oversized");
    std::fs::write(&oversized_wrapper, vec![b'x'; 128 * 1024 + 1]).unwrap();
    std::fs::set_permissions(&oversized_wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::env::set_var("CSSWITCH_TEST_SSH_WRAPPER_OVERRIDE", &oversized_wrapper);
    assert_eq!(
        super::validate_system_ssh_wrapper_path(app.handle()).unwrap_err(),
        "打包的 CSSwitch SSH bridge 不是安全的可执行文件"
    );
    std::fs::remove_dir_all(root).unwrap();
}
