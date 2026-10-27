//! 从 `td_api.tl` 生成 TypeScript 类型声明（`td_api.d.ts`）。
//!
//! 这是 `fuyubot-sdk td-types` 命令背后的生成器：复用 [`TdApiSchema::compiled`]
//! 里那套已经解析好的 schema（`---functions---` / `---types---` 分区、文档注释、
//! 参数/返回类型），产出给 JS/TS 脚本插件作者用的**精确类型**。
//!
//! ## 它解决什么
//!
//! 脚本插件的 `on` / `invoke` 现在只有 `(event: string, handler)` /
//! `(method: string, params)` 这种裸字符串签名——作者写 `on("updateNewMessage")`
//! 时既不知道有哪些事件名，也拿不到回调里 `payload` 的形状。这份 `.d.ts` 把
//! 「事件名 → 负载类型」「方法名 → 参数/返回类型」做成映射，让编辑器能补全。
//!
//! ## 命名（自洽，不必对齐任何第三方）
//!
//! - **类**（若干构造器共用的 `result`，如 `Message`）→ `export type Message = …`
//!   （union，PascalCase，即 tl 里的原名）。
//! - **构造器**（如 `message`、`updateNewMessage`）→ `export interface message { … }`
//!   （**保持 camelCase 原名**，带 `"@type": "原名"` 判别字段）。
//!   这样 `message`（接口）与 `Message`（类）靠大小写区分，永不撞名，而且事件名
//!   `updateNewMessage` 与 `on` 的第一个参数逐字对应。
//!
//! ## 类型引用的两种形态（关键，别漏）
//!
//! tl 里字段类型**有时写类名**（`content:MessageContent`），**有时写构造器名**
//! （`updateNewMessage message:message`）。两者在 JSON 层面等价（靠 `@type` 判别），
//! 所以这里建一张「引用名 → 类名」的表：类名映射到自身，构造器名映射到它的 `result`。
//!
//! ## 类型映射（TL → TS）
//!
//! | TL | TS |
//! |---|---|
//! | `int32` / `int53` / `double` | `number` |
//! | `int64` | `string`（JS `number` 装不下 64 位整数） |
//! | `string` / `bytes` | `string` |
//! | `bool` / `Bool` | `boolean` |
//! | `vector<T>` | `T[]` |
//! | `T?` | `T \| null` |
//! | 类/构造器名 | 对应的 `type` |
//! | 认不出的名字 | `unknown` |

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use fuyubot_tdlib_sys::{EntryKind, TdApiSchema, TlEntry};

/// tl 前奏里定义的标量别名（`int32 = Int32;` 这类），它们的 `result` 不是真类。
const SCALAR_RESULTS: &[&str] = &[
    "Int32", "Int53", "Int64", "Double", "String", "Bytes", "Bool", "Vector",
];

/// 生成整份 `td_api.d.ts` 的正文（不含文件头注释，那由调用方写）。
///
/// 从 [`TdApiSchema::compiled`] 里拿 schema（构建期已烤进二进制），保证与
/// 宿主实际用的 TDLib 版本一致。
pub fn generate() -> String {
    let schema = TdApiSchema::compiled();
    generate_from(schema.entries())
}

/// 从一组条目生成（测试用它注入内联样例，不依赖真 schema）。
pub fn generate_from(entries: &[TlEntry]) -> String {
    let functions: Vec<&TlEntry> = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Function)
        .collect();
    // 真正的构造器：类型段里、且不是标量别名（`int32 = Int32;` 这种）。
    let constructors: Vec<&TlEntry> = entries
        .iter()
        .filter(|entry| entry.kind == EntryKind::Type && !is_scalar_alias(entry))
        .collect();

    // 类（result）→ 变体列表。只认 PascalCase、且不是标量别名的 result。
    let mut classes: BTreeMap<String, Vec<&TlEntry>> = BTreeMap::new();
    for constructor in &constructors {
        classes
            .entry(constructor.result.clone())
            .or_default()
            .push(constructor);
    }
    classes.retain(|name, _| is_class_name(name) && !SCALAR_RESULTS.contains(&name.as_str()));
    let class_names: BTreeSet<String> = classes.keys().cloned().collect();

    // 引用名 → 类名：类名映射自身，构造器名映射其 result。
    let mut type_map: BTreeMap<String, String> = BTreeMap::new();
    for class in &class_names {
        type_map.insert(class.clone(), class.clone());
    }
    for constructor in &constructors {
        if is_class_name(&constructor.result)
            && !SCALAR_RESULTS.contains(&constructor.result.as_str())
        {
            type_map.insert(constructor.name.clone(), constructor.result.clone());
        }
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "// 本文件由 `fuyubot-sdk td-types` 从 td_api.tl 生成，不要手改。"
    );
    let _ = writeln!(
        out,
        "// 条目 {} 条 → 类 {} 个、函数 {} 个。",
        entries.len(),
        classes.len(),
        functions.len()
    );
    let _ = writeln!(out);

    // 1) 每个构造器一个 interface（带 @type 判别）。
    for constructor in &constructors {
        generate_constructor(&mut out, constructor, &type_map);
    }

    // 2) 每个类一个 union type。
    for (class, variants) in &classes {
        generate_class(&mut out, &class, &variants);
    }

    // 3) 每个函数一个参数 interface。
    for function in &functions {
        generate_function_params(&mut out, function, &type_map);
    }

    // 4) 事件映射（update* → 类型）与函数映射（方法名 → 参数/返回）。
    generate_update_map(&mut out, &constructors);
    generate_function_map(&mut out, &functions);

    out
}

/// 一个条目是不是标量别名定义（`int32 = Int32;` / `boolFalse = Bool;`）。
fn is_scalar_alias(entry: &TlEntry) -> bool {
    SCALAR_RESULTS.contains(&entry.result.as_str())
        || matches!(
            entry.name.as_str(),
            "int32"
                | "int53"
                | "int64"
                | "double"
                | "string"
                | "bytes"
                | "boolFalse"
                | "boolTrue"
                | "vector"
        )
}

/// 一个构造器 → 一个 interface。
fn generate_constructor(
    out: &mut String,
    constructor: &TlEntry,
    type_map: &BTreeMap<String, String>,
) {
    if let Some(description) = &constructor.description {
        emit_jsdoc(out, description, "");
    }
    let _ = writeln!(out, "export interface {} {{", constructor.name);
    let _ = writeln!(out, "  \"@type\": \"{}\";", constructor.name);
    for param in &constructor.params {
        if let Some(doc) = constructor.field_doc(&param.name) {
            emit_jsdoc(out, doc, "  ");
        }
        let ty = shape_to_ts(&param.ty, type_map);
        // 字段全可选：TDLib 的 JSON 里缺省字段用默认值，作者只传自己关心的。
        let _ = writeln!(
            out,
            "  {}?: {};",
            js_field_name(&param.name),
            ts_field_type(&param.ty, &ty)
        );
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
}

/// 一个类 → 一个 union type。
fn generate_class(out: &mut String, class: &str, variants: &[&TlEntry]) {
    if let Some(description) = variants
        .iter()
        .find_map(|variant| variant.class_description.as_deref())
    {
        emit_jsdoc(out, description, "");
    }
    let _ = writeln!(
        out,
        "export type {class} = {};",
        variants
            .iter()
            .map(|variant| variant.name.as_str())
            .collect::<Vec<_>>()
            .join(" | ")
    );
    let _ = writeln!(out);
}

/// 一个函数 → 一个参数 interface（`sendMessage` → `sendMessageParams`）。
fn generate_function_params(
    out: &mut String,
    function: &TlEntry,
    type_map: &BTreeMap<String, String>,
) {
    if let Some(description) = &function.description {
        emit_jsdoc(out, description, "");
    }
    let _ = writeln!(out, "export interface {}Params {{", function.name);
    for param in &function.params {
        if let Some(doc) = function.field_doc(&param.name) {
            emit_jsdoc(out, doc, "  ");
        }
        let ty = shape_to_ts(&param.ty, type_map);
        // 参数全可选：TDLib 会填缺省项，作者只传自己需要的。
        let _ = writeln!(
            out,
            "  {}?: {};",
            js_field_name(&param.name),
            ts_field_type(&param.ty, &ty)
        );
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
}

/// `update*` 事件名 → 构造器 interface 名的映射，喂给 `on` 的重载。
fn generate_update_map(out: &mut String, constructors: &[&TlEntry]) {
    let _ = writeln!(out, "// 事件名 → 负载类型。`on` 的第一个参数从这里来。");
    let _ = writeln!(out, "export interface TdUpdateMap {{");
    for constructor in constructors.iter().filter(|c| c.name.starts_with("update")) {
        let _ = writeln!(out, "  {}: {};", constructor.name, constructor.name);
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
}

/// 方法名 → `{ params, result }` 的映射，喂给 `invoke` 的重载。
fn generate_function_map(out: &mut String, functions: &[&TlEntry]) {
    let _ = writeln!(
        out,
        "// 方法名 → 参数/返回类型。`invoke` 的第一个参数从这里来。"
    );
    let _ = writeln!(out, "export interface TdFunctionMap {{");
    for function in functions {
        let result = result_type(function);
        let _ = writeln!(
            out,
            "  {}: {{ params: {}Params; result: {result} }};",
            function.name, function.name
        );
    }
    let _ = writeln!(out, "}}");
    let _ = writeln!(out);
}

/// 函数的返回类型。类名（PascalCase）直接照抄；标量映射成 TS 类型；其余 `unknown`。
fn result_type(function: &TlEntry) -> String {
    match function.result.as_str() {
        "" => "unknown".to_owned(),
        other if is_class_name(other) && !SCALAR_RESULTS.contains(&other) => other.to_owned(),
        other => match other {
            "int32" | "int53" | "double" => "number".to_owned(),
            "int64" => "string".to_owned(),
            "string" | "bytes" => "string".to_owned(),
            "bool" | "Bool" => "boolean".to_owned(),
            _ => "unknown".to_owned(),
        },
    }
}

/// TL 类型引用 → TS 类型（不含可选后缀处理，`T?` 在 `ts_field_type` 里单独处理）。
fn shape_to_ts(ty: &str, type_map: &BTreeMap<String, String>) -> String {
    let ty = ty.trim();
    if let Some(inner) = ty
        .strip_prefix("vector<")
        .and_then(|rest| rest.strip_suffix('>'))
    {
        return format!("{}[]", shape_to_ts(inner, type_map));
    }
    if let Some(inner) = ty.strip_suffix('?') {
        return shape_to_ts(inner, type_map);
    }
    match ty {
        "int32" | "int53" | "double" => "number".to_owned(),
        "int64" => "string".to_owned(),
        "string" | "bytes" => "string".to_owned(),
        "bool" | "Bool" => "boolean".to_owned(),
        other => type_map
            .get(other)
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned()),
    }
}

/// 字段类型：可选（`T?`）的字段标成 `T | null`；其余原样。
fn ts_field_type(tl_ty: &str, base: &str) -> String {
    if tl_ty.trim().ends_with('?') {
        format!("{base} | null")
    } else {
        base.to_owned()
    }
}

/// 类名是 PascalCase；`int53`、`string` 这种结果类型不是类。
fn is_class_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// 字段名转义。TS 里 `delete` / `type` 这类关键字不能当属性名，得加引号。
fn js_field_name(name: &str) -> String {
    const KEYWORDS: &[&str] = &[
        "delete",
        "type",
        "typeof",
        "new",
        "in",
        "instanceof",
        "void",
        "this",
        "super",
        "class",
        "function",
        "return",
        "var",
        "let",
        "const",
        "if",
        "else",
        "for",
        "while",
        "do",
        "switch",
        "case",
        "break",
        "continue",
        "throw",
        "try",
        "catch",
        "finally",
        "debugger",
        "with",
        "default",
        "extends",
        "implements",
        "interface",
        "package",
        "private",
        "protected",
        "public",
        "static",
        "enum",
        "export",
        "import",
        "null",
        "true",
        "false",
    ];
    if KEYWORDS.contains(&name) {
        format!("\"{name}\"")
    } else {
        name.to_owned()
    }
}

/// 把一段 TL 文档转成 JSDoc 注释行。
fn emit_jsdoc(out: &mut String, text: &str, indent: &str) {
    // TL 的 HTML 风格标记剥成纯文本（<b> 之类去掉尖括号）；`@参数` 片段
    // 在 tl_schema 定稿时已经剥走，这里只会看到干净的主文。
    let cleaned = strip_html(text);
    let _ = writeln!(out, "{indent}/** {cleaned} */");
}

/// 剥掉 `<b>` / `<code>` 这类 HTML 标记（只去尖括号，不去内容）。
fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuyubot_tdlib_sys::tl_schema;

    /// 内联样例：覆盖标量别名、构造器名做类型引用、可选、vector、update 事件、函数。
    const SAMPLE: &str = "int32 = Int32;\n\
         int53 = Int53;\n\
         int64 = Int64;\n\
         bytes = Bytes;\n\
         boolFalse = Bool;\n\
         boolTrue = Bool;\n\
         //@class Message @description Describes a message\n\
         //@description A message @id The identifier\n\
         message id:int53 content:MessageContent = Message;\n\
         messageText text:FormattedText = MessageContent;\n\
         formattedText text:string entities:vector<TextEntity> = FormattedText;\n\
         textEntity offset:int32 length:int32 = TextEntity;\n\
         //@description A new incoming message\n\
         updateNewMessage message:message = Update;\n\
         ---functions---\n\
         //@description Returns the current user\n\
         getMe = User;\n\
         //@description Sends a message @chat_id Target chat\n\
         sendMessage chat_id:int53 reply_markup:ReplyMarkup? = Message;\n";

    fn generated() -> String {
        let entries = tl_schema::parse(SAMPLE).unwrap();
        generate_from(&entries)
    }

    #[test]
    fn scalar_aliases_are_not_emitted_as_interfaces() {
        let code = generated();
        // `int32 = Int32;` 这类前奏既不能当构造器 interface，也不能当类。
        for scalar in ["int32", "int53", "int64", "bytes", "boolFalse", "boolTrue"] {
            assert!(
                !code.contains(&format!("export interface {scalar} {{")),
                "{scalar} 不该生成 interface：{code}"
            );
        }
        for alias in ["Int32", "Int53", "Int64", "Bytes", "Bool"] {
            assert!(
                !code.contains(&format!("export type {alias} =")),
                "{alias} 不该生成 union type：{code}"
            );
        }
    }

    #[test]
    fn constructors_become_interfaces_tagged_by_at_type() {
        let code = generated();
        assert!(code.contains("export interface message {"), "{code}");
        assert!(code.contains("\"@type\": \"message\";"), "{code}");
        assert!(
            code.contains("export interface updateNewMessage {"),
            "{code}"
        );
        // 字段文档进了 JSDoc
        assert!(code.contains("/** The identifier */"), "{code}");
    }

    #[test]
    fn constructor_names_as_type_references_resolve_to_their_class() {
        let code = generated();
        // `updateNewMessage message:message` —— 小写 `message` 是构造器名，要解析成类 `Message`。
        assert!(
            code.contains("export interface updateNewMessage {"),
            "{code}"
        );
        assert!(
            code.contains("  message?: Message;"),
            "构造器名做类型引用要解析到类：{code}"
        );
    }

    #[test]
    fn vectors_and_optionals_map_to_ts() {
        let code = generated();
        // vector<T> → T[]；T? → T | null；字段全可选（TDLib 缺省填默认值）。
        assert!(code.contains("entities?: TextEntity[];"), "{code}");
        // `ReplyMarkup` 在样例里没有构造器，降级成 unknown，但可选后缀仍标 `| null`。
        assert!(code.contains("reply_markup?: unknown | null;"), "{code}");
    }

    #[test]
    fn classes_become_union_types() {
        let code = generated();
        assert!(code.contains("export type Message = message;"), "{code}");
        assert!(
            code.contains("export type MessageContent = messageText;"),
            "{code}"
        );
    }

    #[test]
    fn functions_become_param_interfaces_and_a_map() {
        let code = generated();
        assert!(
            code.contains("export interface sendMessageParams {"),
            "{code}"
        );
        assert!(code.contains("  chat_id?: number;"), "{code}");
        assert!(code.contains("export interface getMeParams {"), "{code}");
        // 函数映射：方法名 → { params, result }
        assert!(
            code.contains("  sendMessage: { params: sendMessageParams; result: Message };"),
            "{code}"
        );
        // `getMe = User`：User 是 PascalCase 的类名，直接照抄（即使样例里没定义它的构造器）。
        assert!(
            code.contains("  getMe: { params: getMeParams; result: User };"),
            "{code}"
        );
    }

    #[test]
    fn update_events_collect_into_the_update_map() {
        let code = generated();
        assert!(code.contains("export interface TdUpdateMap {"), "{code}");
        assert!(
            code.contains("  updateNewMessage: updateNewMessage;"),
            "{code}"
        );
    }

    #[test]
    fn keywords_are_quoted_as_field_names() {
        let entries = tl_schema::parse(
            "---types---\nfoo type:string = Foo;\n---functions---\nbar delete:int32 = Ok;\n",
        )
        .unwrap();
        let code = generate_from(&entries);
        assert!(code.contains("\"type\"?: string;"), "{code}");
        assert!(code.contains("\"delete\"?: number;"), "{code}");
    }
}
