// External scanner for tree-sitter-pascal.
//
// Recognizes single-line `{$ifdef ...}...{$endif}` directive pairs (also
// spelled `(*$ifdef ...*)...(*$endif*)`) and
// consumes the whole paired span as ONE opaque token — either
// `ppFragmentExpr` (the default, valid in expression and typeref
// positions) or `ppFragmentStmt` (when the body contains a top-level `;`
// AND the grammar accepts a statement fragment at this position).
// Returns false (letting the regex-based lexer handle the input) when
// the directive is followed by whitespace/newline — that's the
// block-level form handled by ppBlock / pp().
//
// Also recognizes Delphi 12 multiline strings: an odd run of at least three
// quotes ending its line, closed by the same run at the start of a later
// line (after optional indentation).

#include "tree_sitter/parser.h"
#include <ctype.h>
#include <stdbool.h>
#include <stddef.h>
#include <string.h>

typedef enum {
    PP_FRAGMENT_EXPR,
    PP_FRAGMENT_STMT,
    LITERAL_STRING_MULTILINE,
    IDENTIFIER,
    KCONSTREF,
    KOUT,
    KABSOLUTE,
    KREAD,
    KWRITE,
    KIMPLEMENTS,
    KDEFAULT,
    KNODEFAULT,
    KSTORED,
    KINDEX,
    KDISPID,
    KOBJCCLASS,
    KOBJCCATEGORY,
    KOBJCPROTOCOL,
    KHELPER,
    KGENERIC,
    KSPECIALIZE,
    KOTHERWISE,
    KRAISEAT,
    KON,
    KOPERATOR,
    KREFERENCE,
    KPUBLISHED,
    KPUBLIC,
    KPROTECTED,
    KPRIVATE,
    KSTRICT,
    KREQUIRED,
    KOPTIONAL,
    KFORWARD,
    KSTATIC,
    KVIRTUAL,
    KABSTRACT,
    KSEALED,
    KDYNAMIC,
    KOVERRIDE,
    KOVERLOAD,
    KREINTRODUCE,
    KSTDCALL,
    KCDECL,
    KCPPDECL,
    KPASCAL,
    KREGISTER,
    KMWPASCAL,
    KEXTERNAL,
    KNAME,
    KMESSAGE,
    KDEPRECATED,
    KEXPERIMENTAL,
    KPLATFORM,
    KUNIMPLEMENTED,
    KCVAR,
    KEXPORT,
    KFAR,
    KNEAR,
    KSAFECALL,
    KASSEMBLER,
    KNOSTACKFRAME,
    KINTERRUPT,
    KNORETURN,
    KIOCHECK,
    KLOCAL,
    KHARDFLOAT,
    KSOFTFLOAT,
    KMS_ABI_DEFAULT,
    KMS_ABI_CDECL,
    KSAVEREGISTERS,
    KSYSV_ABI_DEFAULT,
    KSYSV_ABI_CDECL,
    KVECTORCALL,
    KVARARGS,
    KWINAPI,
    KALIAS,
    KDELAYED,
    KTRUE,
    KFALSE,
    KAND,
    KARRAY,
    KAS,
    KASM,
    KBEGIN,
    KCASE,
    KCLASS,
    KCONST,
    KCONSTRUCTOR,
    KDESTRUCTOR,
    KDISPINTERFACE,
    KDIV,
    KDO,
    KDOWNTO,
    KELSE,
    KEND,
    KEXCEPT,
    KEXPORTS,
    KFILE,
    KFINALIZATION,
    KFINALLY,
    KFOR,
    KFUNCTION,
    KGOTO,
    KIF,
    KIMPLEMENTATION,
    KIN,
    KINHERITED,
    KINITIALIZATION,
    KINLINE,
    KINTERFACE,
    KIS,
    KLABEL,
    KLIBRARY,
    KMOD,
    KNIL,
    KNOT,
    KOBJECT,
    KOF,
    KOR,
    KPACKED,
    KPROCEDURE,
    KPROGRAM,
    KPROPERTY,
    KRAISE,
    KRECORD,
    KREPEAT,
    KRESOURCESTRING,
    KSET,
    KSHL,
    KSHR,
    KSTRING,
    KTHEN,
    KTHREADVAR,
    KTO,
    KTRY,
    KTYPE,
    KUNIT,
    KUNTIL,
    KUSES,
    KVAR,
    KWHILE,
    KWITH,
    KXOR,
} TokenType;

static inline bool is_ascii_letter(int32_t c) {
    return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z');
}

typedef struct {
    const char *text;
    TokenType token;
} ContextualWord;

typedef struct {
    const char *text;
    TokenType token;
} DccReservedWord;

static const ContextualWord CONTEXTUAL_WORDS[] = {
    {"constref", KCONSTREF},
    {"out", KOUT},
    {"absolute", KABSOLUTE},
    {"read", KREAD},
    {"write", KWRITE},
    {"implements", KIMPLEMENTS},
    {"default", KDEFAULT},
    {"nodefault", KNODEFAULT},
    {"stored", KSTORED},
    {"index", KINDEX},
    {"dispid", KDISPID},
    {"objcclass", KOBJCCLASS},
    {"objccategory", KOBJCCATEGORY},
    {"objcprotocol", KOBJCPROTOCOL},
    {"helper", KHELPER},
    {"generic", KGENERIC},
    {"specialize", KSPECIALIZE},
    {"otherwise", KOTHERWISE},
    {"at", KRAISEAT},
    {"on", KON},
    {"operator", KOPERATOR},
    {"reference", KREFERENCE},
    {"published", KPUBLISHED},
    {"public", KPUBLIC},
    {"protected", KPROTECTED},
    {"private", KPRIVATE},
    {"strict", KSTRICT},
    {"required", KREQUIRED},
    {"optional", KOPTIONAL},
    {"forward", KFORWARD},
    {"static", KSTATIC},
    {"virtual", KVIRTUAL},
    {"abstract", KABSTRACT},
    {"sealed", KSEALED},
    {"dynamic", KDYNAMIC},
    {"override", KOVERRIDE},
    {"overload", KOVERLOAD},
    {"reintroduce", KREINTRODUCE},
    {"stdcall", KSTDCALL},
    {"cdecl", KCDECL},
    {"cppdecl", KCPPDECL},
    {"pascal", KPASCAL},
    {"register", KREGISTER},
    {"mwpascal", KMWPASCAL},
    {"external", KEXTERNAL},
    {"name", KNAME},
    {"message", KMESSAGE},
    {"deprecated", KDEPRECATED},
    {"experimental", KEXPERIMENTAL},
    {"platform", KPLATFORM},
    {"unimplemented", KUNIMPLEMENTED},
    {"cvar", KCVAR},
    {"export", KEXPORT},
    {"far", KFAR},
    {"near", KNEAR},
    {"safecall", KSAFECALL},
    {"assembler", KASSEMBLER},
    {"nostackframe", KNOSTACKFRAME},
    {"interrupt", KINTERRUPT},
    {"noreturn", KNORETURN},
    {"iocheck", KIOCHECK},
    {"local", KLOCAL},
    {"hardfloat", KHARDFLOAT},
    {"softfloat", KSOFTFLOAT},
    {"ms_abi_default", KMS_ABI_DEFAULT},
    {"ms_abi_cdecl", KMS_ABI_CDECL},
    {"saveregisters", KSAVEREGISTERS},
    {"sysv_abi_default", KSYSV_ABI_DEFAULT},
    {"sysv_abi_cdecl", KSYSV_ABI_CDECL},
    {"vectorcall", KVECTORCALL},
    {"varargs", KVARARGS},
    {"winapi", KWINAPI},
    {"alias", KALIAS},
    {"delayed", KDELAYED},
    {"true", KTRUE},
    {"false", KFALSE},
};

static const DccReservedWord DCC_RESERVED_WORDS[] = {
    {"and", KAND},
    {"array", KARRAY},
    {"as", KAS},
    {"asm", KASM},
    {"begin", KBEGIN},
    {"case", KCASE},
    {"class", KCLASS},
    {"const", KCONST},
    {"constructor", KCONSTRUCTOR},
    {"destructor", KDESTRUCTOR},
    {"dispinterface", KDISPINTERFACE},
    {"div", KDIV},
    {"do", KDO},
    {"downto", KDOWNTO},
    {"else", KELSE},
    {"end", KEND},
    {"except", KEXCEPT},
    {"exports", KEXPORTS},
    {"file", KFILE},
    {"finalization", KFINALIZATION},
    {"finally", KFINALLY},
    {"for", KFOR},
    {"function", KFUNCTION},
    {"goto", KGOTO},
    {"if", KIF},
    {"implementation", KIMPLEMENTATION},
    {"in", KIN},
    {"inherited", KINHERITED},
    {"initialization", KINITIALIZATION},
    {"inline", KINLINE},
    {"interface", KINTERFACE},
    {"is", KIS},
    {"label", KLABEL},
    {"library", KLIBRARY},
    {"mod", KMOD},
    {"nil", KNIL},
    {"not", KNOT},
    {"object", KOBJECT},
    {"of", KOF},
    {"or", KOR},
    {"packed", KPACKED},
    {"procedure", KPROCEDURE},
    {"program", KPROGRAM},
    {"property", KPROPERTY},
    {"raise", KRAISE},
    {"record", KRECORD},
    {"repeat", KREPEAT},
    {"resourcestring", KRESOURCESTRING},
    {"set", KSET},
    {"shl", KSHL},
    {"shr", KSHR},
    {"string", KSTRING},
    {"then", KTHEN},
    {"threadvar", KTHREADVAR},
    {"to", KTO},
    {"try", KTRY},
    {"type", KTYPE},
    {"unit", KUNIT},
    {"until", KUNTIL},
    {"uses", KUSES},
    {"var", KVAR},
    {"while", KWHILE},
    {"with", KWITH},
    {"xor", KXOR},
};

static inline bool is_identifier_start(int32_t c) {
    return is_ascii_letter(c) || c == '_';
}

static inline bool is_identifier_continue(int32_t c) {
    return is_identifier_start(c) || (c >= '0' && c <= '9') || c == '$';
}

typedef enum {
    WORD_NOT_APPLICABLE,
    WORD_NO_TOKEN,
    WORD_EMITTED,
} WordScanResult;

static const DccReservedWord *find_dcc_reserved_word(const char *word, size_t len) {
    for (size_t i = 0; i < sizeof(DCC_RESERVED_WORDS) / sizeof(DCC_RESERVED_WORDS[0]); i++) {
        const DccReservedWord *candidate = &DCC_RESERVED_WORDS[i];
        if (strlen(candidate->text) == len && memcmp(candidate->text, word, len) == 0) {
            return candidate;
        }
    }
    return NULL;
}

static WordScanResult scan_word_token(TSLexer *lexer, const bool *valid_symbols) {
    bool escaped = lexer->lookahead == '&';
    if (escaped) {
        lexer->advance(lexer, false);
    }
    if (!is_identifier_start(lexer->lookahead)) {
        return WORD_NOT_APPLICABLE;
    }

    char word[32] = {0};
    size_t len = 0;
    bool too_long = false;
    while (is_identifier_continue(lexer->lookahead)) {
        int32_t c = lexer->lookahead;
        if (len < sizeof(word) - 1) {
            word[len++] = (char)(is_ascii_letter(c) ? tolower((unsigned char)c) : c);
        } else {
            too_long = true;
        }
        lexer->advance(lexer, false);
    }
    lexer->mark_end(lexer);

    if (!escaped && !too_long) {
        for (size_t i = 0; i < sizeof(CONTEXTUAL_WORDS) / sizeof(CONTEXTUAL_WORDS[0]); i++) {
            const ContextualWord *candidate = &CONTEXTUAL_WORDS[i];
            if (strlen(candidate->text) == len && memcmp(candidate->text, word, len) == 0 &&
                valid_symbols[candidate->token]) {
                lexer->result_symbol = candidate->token;
                return WORD_EMITTED;
            }
        }
        const DccReservedWord *reserved = find_dcc_reserved_word(word, len);
        if (reserved) {
            if (valid_symbols[reserved->token]) {
                lexer->result_symbol = reserved->token;
                return WORD_EMITTED;
            }
            return WORD_NO_TOKEN;
        }
    }

    if (valid_symbols[IDENTIFIER]) {
        lexer->result_symbol = IDENTIFIER;
        return WORD_EMITTED;
    }
    return WORD_NO_TOKEN;
}

static inline bool is_space_or_newline(int32_t c) {
    return c == ' ' || c == '\t' || c == '\r' || c == '\n';
}

// Read up to `cap` ASCII letters into `buf`, lowercased. Stops at the
// first non-letter. Returns the number of letters read. Advances the
// lexer past every letter consumed.
static size_t read_ascii_keyword(TSLexer *lexer, char *buf, size_t cap) {
    size_t len = 0;
    while (len < cap && is_ascii_letter(lexer->lookahead)) {
        buf[len++] = (char)tolower((unsigned char)lexer->lookahead);
        lexer->advance(lexer, false);
    }
    return len;
}

// Routine directives that take no arguments. A fragment holding only these
// (e.g. `{$IFDEF FPC} assembler; nostackframe; {$ENDIF}` after a routine
// header) is left to the grammar's directive handling instead.
static const char *const ROUTINE_DIRECTIVES[] = {
    "abstract", "assembler", "cdecl", "cppdecl", "deprecated", "dynamic",
    "experimental", "far", "hardfloat", "inline", "interrupt", "iocheck",
    "local", "mwpascal", "near", "noreturn", "nostackframe", "overload",
    "override", "pascal", "platform", "register", "reintroduce", "safecall",
    "saveregisters", "softfloat", "static", "stdcall", "unimplemented",
    "varargs", "vectorcall", "virtual", "winapi",
};

static bool is_routine_directive(const char *word, size_t len) {
    for (size_t i = 0; i < sizeof(ROUTINE_DIRECTIVES) / sizeof(ROUTINE_DIRECTIVES[0]); i++) {
        if (strlen(ROUTINE_DIRECTIVES[i]) == len && memcmp(ROUTINE_DIRECTIVES[i], word, len) == 0) {
            return true;
        }
    }
    return false;
}

// Skip input up to and including the end of a directive or comment: the
// next `}`, or the next `*)` if it opened with `(*`. Sets `*saw_newline`
// (when not NULL) if a line break is skipped. Returns false on EOF.
static bool skip_to_close(TSLexer *lexer, bool paren_star, bool *saw_newline) {
    while (lexer->lookahead != 0) {
        int32_t c = lexer->lookahead;
        lexer->advance(lexer, false);
        if (saw_newline && (c == '\n' || c == '\r')) {
            *saw_newline = true;
        }
        if (paren_star ? c == '*' && lexer->lookahead == ')' : c == '}') {
            if (paren_star) {
                lexer->advance(lexer, false);
            }
            return true;
        }
    }
    return false;
}

static inline bool is_blank(int32_t c) {
    return c == ' ' || c == '\t';
}

static unsigned count_quotes(TSLexer *lexer) {
    unsigned count = 0;
    while (lexer->lookahead == '\'') {
        count++;
        lexer->advance(lexer, false);
    }
    return count;
}

static bool scan_multiline_string(TSLexer *lexer) {
    unsigned delimiter = count_quotes(lexer);
    if (delimiter < 3 || delimiter % 2 == 0) {
        return false;
    }
    while (is_blank(lexer->lookahead)) {
        lexer->advance(lexer, false);
    }
    if (lexer->lookahead != '\r' && lexer->lookahead != '\n') {
        return false;
    }

    for (;;) {
        // Consume the rest of the current line, including its line break.
        while (lexer->lookahead != '\n') {
            if (lexer->eof(lexer)) {
                return false;
            }
            lexer->advance(lexer, false);
        }
        lexer->advance(lexer, false);

        while (is_blank(lexer->lookahead)) {
            lexer->advance(lexer, false);
        }
        if (count_quotes(lexer) == delimiter) {
            lexer->mark_end(lexer);
            lexer->result_symbol = LITERAL_STRING_MULTILINE;
            return true;
        }
    }
}

void *tree_sitter_pascal_external_scanner_create(void) {
    return NULL;
}

void tree_sitter_pascal_external_scanner_destroy(void *payload) {
    (void)payload;
}

unsigned tree_sitter_pascal_external_scanner_serialize(void *payload, char *buffer) {
    (void)payload;
    (void)buffer;
    return 0;
}

void tree_sitter_pascal_external_scanner_deserialize(
    void *payload,
    const char *buffer,
    unsigned length
) {
    (void)payload;
    (void)buffer;
    (void)length;
}

bool tree_sitter_pascal_external_scanner_scan(
    void *payload,
    TSLexer *lexer,
    const bool *valid_symbols
) {
    (void)payload;

    if (valid_symbols[LITERAL_STRING_MULTILINE] && lexer->lookahead == '\'') {
        return scan_multiline_string(lexer);
    }

    bool has_word_symbol = valid_symbols[IDENTIFIER];
    for (size_t i = 0; i < sizeof(CONTEXTUAL_WORDS) / sizeof(CONTEXTUAL_WORDS[0]); i++) {
        has_word_symbol = has_word_symbol || valid_symbols[CONTEXTUAL_WORDS[i].token];
    }
    for (size_t i = 0; i < sizeof(DCC_RESERVED_WORDS) / sizeof(DCC_RESERVED_WORDS[0]); i++) {
        has_word_symbol = has_word_symbol || valid_symbols[DCC_RESERVED_WORDS[i].token];
    }
    if (has_word_symbol) {
        WordScanResult result = scan_word_token(lexer, valid_symbols);
        if (result == WORD_EMITTED) {
            return true;
        }
        if (result == WORD_NO_TOKEN) {
            return false;
        }
    }

    if (!valid_symbols[PP_FRAGMENT_EXPR] && !valid_symbols[PP_FRAGMENT_STMT]) {
        return false;
    }

    // Must start with `{$` or `(*$`.
    bool paren_star = lexer->lookahead == '(';
    if (lexer->lookahead != '{' && !paren_star) {
        return false;
    }
    lexer->advance(lexer, false);
    if (paren_star) {
        if (lexer->lookahead != '*') {
            return false;
        }
        lexer->advance(lexer, false);
    }
    if (lexer->lookahead != '$') {
        return false;
    }
    lexer->advance(lexer, false);

    // The opening keyword must be `if`, `ifdef`, or `ifndef`.
    char keyword[8] = {0};
    size_t keyword_len = read_ascii_keyword(lexer, keyword, sizeof(keyword) - 1);
    if (!(
        (keyword_len == 2 && memcmp(keyword, "if", 2) == 0) ||
        (keyword_len == 5 && memcmp(keyword, "ifdef", 5) == 0) ||
        (keyword_len == 6 && memcmp(keyword, "ifndef", 6) == 0)
    )) {
        return false;
    }

    // Consume up to and including the end of the opening directive.
    if (!skip_to_close(lexer, paren_star, NULL)) {
        return false;
    }

    // Track whether any newline appears inside the `{$if*}...{$endif}` span.
    // A directive body that contains a newline is structural (block-level)
    // and is handled by the regex-based lexer via `pp()` / `ppBlock`; fragments
    // by definition fit on a single physical line. The `valid_symbols[PP_FRAGMENT_EXPR]`
    // gate at the top of this function already prevents firing in positions
    // where ppFragmentExpr isn't grammatically valid, so no additional "mid-line
    // content" check is required.
    bool saw_newline = false;
    bool saw_top_level_semi = false;
    bool saw_directive = false;
    bool directives_only = true;

    // Walk forward to the matching `{$endif}` / `{$ifend}`, tracking depth
    // for nested `{$if*}` pairs.
    unsigned depth = 1;
    while (depth > 0) {
        if (lexer->lookahead == 0) {
            return false; // Unterminated fragment — give up, let regex handle it.
        }
        bool inner_paren_star = lexer->lookahead == '(';
        if (lexer->lookahead != '{' && !inner_paren_star) {
            if (depth == 1 && is_ascii_letter(lexer->lookahead)) {
                char word[16] = {0};
                size_t word_len = read_ascii_keyword(lexer, word, sizeof(word) - 1);
                if (is_ascii_letter(lexer->lookahead) || !is_routine_directive(word, word_len)) {
                    directives_only = false;
                } else {
                    saw_directive = true;
                }
                continue;
            }
            if (lexer->lookahead != ';' && !is_space_or_newline(lexer->lookahead)) {
                directives_only = false;
            }
            if (lexer->lookahead == '\'') {
                // A string: what it holds is not a directive. A line break
                // ends an unterminated one.
                lexer->advance(lexer, false);
                while (lexer->lookahead != '\'' && lexer->lookahead != 0 &&
                       lexer->lookahead != '\n' && lexer->lookahead != '\r') {
                    lexer->advance(lexer, false);
                }
                if (lexer->lookahead == '\'') {
                    lexer->advance(lexer, false);
                }
                continue;
            }
            if (lexer->lookahead == '/') {
                lexer->advance(lexer, false);
                if (lexer->lookahead == '/') {
                    // A `//` comment runs to the line break.
                    while (lexer->lookahead != 0 && lexer->lookahead != '\n' &&
                           lexer->lookahead != '\r') {
                        lexer->advance(lexer, false);
                    }
                }
                continue;
            }
            if (lexer->lookahead == '\n' || lexer->lookahead == '\r') {
                saw_newline = true;
            } else if (lexer->lookahead == ';' && depth == 1) {
                // Deliberately naive: no filtering for `;` inside parens. A
                // Multidev corpus probe (247 single-line fragment spans across
                // 301 .pas files) found zero such cases, so the added
                // complexity has no ROI.
                saw_top_level_semi = true;
            }
            lexer->advance(lexer, false);
            continue;
        }
        lexer->advance(lexer, false);
        if (inner_paren_star) {
            if (lexer->lookahead != '*') {
                directives_only = false; // A parenthesis.
                continue;
            }
            lexer->advance(lexer, false);
        }
        if (lexer->lookahead != '$') {
            // A comment: what it holds is not a directive.
            directives_only = false;
            if (!skip_to_close(lexer, inner_paren_star, &saw_newline)) {
                return false;
            }
            continue;
        }
        lexer->advance(lexer, false);

        char inner[8] = {0};
        size_t inner_len = read_ascii_keyword(lexer, inner, sizeof(inner) - 1);
        if (
            (inner_len == 2 && memcmp(inner, "if", 2) == 0) ||
            (inner_len == 5 && memcmp(inner, "ifdef", 5) == 0) ||
            (inner_len == 6 && memcmp(inner, "ifndef", 6) == 0)
        ) {
            depth++;
            directives_only = false;
        } else if (
            (inner_len == 5 && memcmp(inner, "endif", 5) == 0) ||
            (inner_len == 5 && memcmp(inner, "ifend", 5) == 0)
        ) {
            depth--;
        }

        if (!skip_to_close(lexer, inner_paren_star, NULL)) {
            return false;
        }
    }

    if (saw_newline || (saw_directive && directives_only)) {
        return false;
    }

    // Token selection: prefer PP_FRAGMENT_STMT when the body contains a
    // top-level `;` AND the grammar accepts a statement fragment here.
    // Otherwise fall through to PP_FRAGMENT_EXPR (today's behavior),
    // including the trailing identifier-chain extension pass. Statement
    // fragments are self-contained — their body ends at the closing
    // `{$endif}` so there's nothing to absorb afterwards.
    if (saw_top_level_semi && valid_symbols[PP_FRAGMENT_STMT]) {
        lexer->mark_end(lexer);
        lexer->result_symbol = PP_FRAGMENT_STMT;
        return true;
    }

    if (!valid_symbols[PP_FRAGMENT_EXPR]) {
        return false;
    }

    // Extension pass: after the depth loop reaches 0 we're positioned
    // immediately after the closing `}` of the final `{$endif}`. Real
    // Delphi code uses fragments as PREFIXES to compound identifier
    // chains — e.g. `{$ifdef X}System.{$endif}SysUtils.FreeAndNil` —
    // where the text after the closing directive forms one logical
    // ref/typeref with the fragment. Swallow any trailing identifier-
    // chain characters (letters, digits, underscores, dots) into the
    // same ppFragmentExpr token so the grammar sees a single leaf at the
    // expected position. Stops at the first char that isn't part of an
    // identifier chain: whitespace, newline, EOF, punctuation, `(`,
    // `;`, `:`, `,`, `{`, etc.
    while (true) {
        int32_t c = lexer->lookahead;
        if (c == 0) break;
        if (is_space_or_newline(c)) break;
        if (is_ascii_letter(c) || (c >= '0' && c <= '9') || c == '_' || c == '.') {
            lexer->advance(lexer, false);
            continue;
        }
        break;
    }

    lexer->mark_end(lexer);
    lexer->result_symbol = PP_FRAGMENT_EXPR;
    return true;
}
