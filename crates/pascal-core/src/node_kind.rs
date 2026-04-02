//! Tree-sitter node kind constants for the Pascal grammar.
//!
//! Centralises the magic strings returned by `Node::kind()` so that every
//! comparison in `fmt4d` and `lint4d` goes through a named constant.
//! Import with `use pascal_core::node_kind as K;` for concise usage.

// ── Structural nodes ────────────────────────────────────────────────

pub const UNIT: &str = "unit";
pub const PROGRAM: &str = "program";
pub const LIBRARY: &str = "library";
pub const INTERFACE: &str = "interface";
pub const IMPLEMENTATION: &str = "implementation";
pub const INITIALIZATION: &str = "initialization";
pub const FINALIZATION: &str = "finalization";
pub const BLOCK: &str = "block";
pub const STATEMENTS: &str = "statements";
pub const COMMENT: &str = "comment";

// ── Declaration nodes ───────────────────────────────────────────────

pub const DECL_USES: &str = "declUses";
pub const DECL_CLASS: &str = "declClass";
pub const DECL_RECORD: &str = "declRecord";
pub const DECL_INTF: &str = "declIntf";
pub const DECL_SECTION: &str = "declSection";
pub const DECL_VARS: &str = "declVars";
pub const DECL_CONSTS: &str = "declConsts";
pub const DECL_TYPES: &str = "declTypes";
pub const DECL_VAR: &str = "declVar";
pub const DECL_CONST: &str = "declConst";
pub const DECL_TYPE: &str = "declType";
pub const DECL_FIELD: &str = "declField";
pub const DECL_ARG: &str = "declArg";
pub const DECL_ARGS: &str = "declArgs";
pub const DECL_PROC: &str = "declProc";
pub const DEF_PROC: &str = "defProc";

// ── Expression nodes ────────────────────────────────────────────────

pub const EXPR_CALL: &str = "exprCall";
pub const EXPR_DOT: &str = "exprDot";
pub const EXPR_UNARY: &str = "exprUnary";
pub const EXPR_BINARY: &str = "exprBinary";
pub const EXPR_SUBSCRIPT: &str = "exprSubscript";
pub const EXPR_PARENS: &str = "exprParens";
pub const LAMBDA: &str = "lambda";
pub const GENERIC_DOT: &str = "genericDot";
pub const ASSIGNMENT: &str = "assignment";

// ── Control flow nodes ──────────────────────────────────────────────

pub const IF: &str = "if";
pub const IF_ELSE: &str = "ifElse";
pub const CASE: &str = "case";
pub const CASE_CASE: &str = "caseCase";
pub const TRY: &str = "try";
pub const REPEAT: &str = "repeat";
pub const FOR: &str = "for";
pub const FOREACH: &str = "foreach";
pub const WHILE: &str = "while";
pub const WITH: &str = "with";
pub const RAISE: &str = "raise";
pub const INHERITED: &str = "inherited";
pub const EXCEPTION_HANDLER: &str = "exceptionHandler";
pub const CONDITION: &str = "condition";
pub const CONDITIONAL: &str = "conditional";

// ── Type / identifier nodes ─────────────────────────────────────────

pub const IDENTIFIER: &str = "identifier";
pub const TYPEREF: &str = "typeref";
pub const MODULE_NAME: &str = "moduleName";
pub const TYPE: &str = "type";
pub const PROC_ATTRIBUTE: &str = "procAttribute";
pub const HEADER: &str = "header";
pub const GENERIC_ARGS: &str = "genericArgs";
pub const GENERIC_TPL: &str = "genericTpl";
pub const TYPEREF_TPL: &str = "typerefTpl";
pub const TYPEREF_ARGS: &str = "typerefArgs";
pub const EXPR_TPL: &str = "exprTpl";

// ── Literal nodes ───────────────────────────────────────────────────

pub const LITERAL_CHAR: &str = "literalChar";
pub const LITERAL_STRING: &str = "literalString";
pub const INTEGER: &str = "integer";

// ── Keyword tokens ──────────────────────────────────────────────────

// Arithmetic / logical / bitwise operators
pub const K_ADD: &str = "kAdd";
pub const K_SUB: &str = "kSub";
pub const K_MUL: &str = "kMul";
pub const K_DIV: &str = "kDiv";
pub const K_MOD: &str = "kMod";
pub const K_AND: &str = "kAnd";
pub const K_OR: &str = "kOr";
pub const K_XOR: &str = "kXor";
pub const K_SHL: &str = "kShl";
pub const K_SHR: &str = "kShr";
pub const K_NOT: &str = "kNot";

// Assignment operators
pub const K_ASSIGN: &str = "kAssign";
pub const K_ASSIGN_ADD: &str = "kAssignAdd";
pub const K_ASSIGN_SUB: &str = "kAssignSub";
pub const K_ASSIGN_MUL: &str = "kAssignMul";
pub const K_ASSIGN_DIV: &str = "kAssignDiv";

// Comparison / relational
pub const K_IN: &str = "kIn";
pub const K_IS: &str = "kIs";
pub const K_AS: &str = "kAs";
pub const K_LT: &str = "kLt";
pub const K_GT: &str = "kGt";

// Block / control-flow keywords
pub const K_BEGIN: &str = "kBegin";
pub const K_END: &str = "kEnd";
pub const K_TRY: &str = "kTry";
pub const K_REPEAT: &str = "kRepeat";
pub const K_ASM: &str = "kAsm";
pub const K_EXCEPT: &str = "kExcept";
pub const K_FINALLY: &str = "kFinally";
pub const K_IF: &str = "kIf";
pub const K_THEN: &str = "kThen";
pub const K_ELSE: &str = "kElse";
pub const K_DO: &str = "kDo";
pub const K_WHILE: &str = "kWhile";
pub const K_FOR: &str = "kFor";
pub const K_TO: &str = "kTo";
pub const K_DOWNTO: &str = "kDownto";
pub const K_UNTIL: &str = "kUntil";
pub const K_CASE: &str = "kCase";
pub const K_OF: &str = "kOf";
pub const K_ON: &str = "kOn";
pub const K_WITH: &str = "kWith";

// Declaration keywords
pub const K_VAR: &str = "kVar";
pub const K_CONST: &str = "kConst";
pub const K_TYPE: &str = "kType";
pub const K_USES: &str = "kUses";

// Type keywords
pub const K_CLASS: &str = "kClass";
pub const K_RECORD: &str = "kRecord";
pub const K_INTERFACE: &str = "kInterface";
pub const K_OBJECT: &str = "kObject";
pub const K_ARRAY: &str = "kArray";
pub const K_SET: &str = "kSet";
pub const K_STRING: &str = "kString";
pub const K_FILE: &str = "kFile";
pub const K_PACKED: &str = "kPacked";
pub const K_ABSTRACT: &str = "kAbstract";
pub const K_SEALED: &str = "kSealed";

// Visibility keywords
pub const K_PUBLIC: &str = "kPublic";
pub const K_PRIVATE: &str = "kPrivate";
pub const K_PROTECTED: &str = "kProtected";
pub const K_PUBLISHED: &str = "kPublished";
pub const K_STRICT: &str = "kStrict";

// Procedure / method keywords
pub const K_PROCEDURE: &str = "kProcedure";
pub const K_FUNCTION: &str = "kFunction";
pub const K_CONSTRUCTOR: &str = "kConstructor";
pub const K_DESTRUCTOR: &str = "kDestructor";
pub const K_PROPERTY: &str = "kProperty";

// Section keywords
pub const K_UNIT: &str = "kUnit";
pub const K_PROGRAM: &str = "kProgram";
pub const K_LIBRARY: &str = "kLibrary";
pub const K_IMPLEMENTATION: &str = "kImplementation";
pub const K_INITIALIZATION: &str = "kInitialization";
pub const K_FINALIZATION: &str = "kFinalization";

// Misc keywords
pub const K_RAISE: &str = "kRaise";
pub const K_INHERITED: &str = "kInherited";
pub const K_REINTRODUCE: &str = "kReintroduce";
pub const K_OUT: &str = "kOut";
pub const K_DOT: &str = "kDot";

// ── Punctuation tokens ──────────────────────────────────────────────

pub const SEMICOLON: &str = ";";
pub const COMMA: &str = ",";
pub const COLON: &str = ":";
pub const DOT: &str = ".";
pub const DOTDOT: &str = "..";
pub const OPEN_PAREN: &str = "(";
pub const CLOSE_PAREN: &str = ")";
pub const OPEN_BRACKET: &str = "[";
pub const CLOSE_BRACKET: &str = "]";
pub const EQUALS: &str = "=";
pub const NOT_EQUALS: &str = "<>";
pub const LESS_THAN: &str = "<";
pub const GREATER_THAN: &str = ">";
pub const LESS_EQUAL: &str = "<=";
pub const GREATER_EQUAL: &str = ">=";

// ── Grammar tokens used only in match-skipping ──────────────────────

pub const K_OPEN: &str = "kOpen";
pub const K_CLOSE: &str = "kClose";
pub const K_COMMA: &str = "kComma";
