# tree-sitter-pascal

Grammar for Pascal and its dialects Delphi and Freepascal.

## Supported language features
- Classes, records, interfaces, class helpers
- Nested declarations
- Variant records
- Generics (Delphi- & FPC flavored)
- Anonymous procedures & functions
- Inline assember (but no highlighting)
- Extended RTTI attributes
- FPC PasCocoa extensions
- Typed/inferred Delphi `for var ... in` iterators
- Delphi conditional expressions (`if ... then ... else ...`), `is not`, and `not in`
- Custom managed-record operators, including `const [ref]` parameters
- Binary literals (`%1010`, also in character codes) and FPC `case ... otherwise`
- Delphi digit separators (`1_000_000`, `$FFFF_FFFF`, `%1010_0101`)

### Modern syntax and licensing

The modern syntax additions in this MIT-licensed fork are implemented independently
from language documentation, not copied from AGPL-licensed forks. References:

- [Delphi inline declarations](https://blogs.embarcadero.com/introducing-inline-variables-in-the-delphi-language/)
- [Delphi conditional expressions and precedence](https://blogs.embarcadero.com/coming-in-rad-studio-13-a-conditional-ternary-operator-for-the-delphi-language/)
- [Delphi 13 language features](https://blogs.embarcadero.com/rad-studio-13-every-new-and-enhanced-feature/)
- [Custom managed records](https://blogs.embarcadero.com/custom-managed-records-coming-to-delphi-10-4/)
- [FPC numeric literals](https://www.freepascal.org/docs-html/current/ref/refse6.html)
- [Delphi numerals and digit separators](https://docwiki.embarcadero.com/RADStudio/Florence/en/Fundamental_Syntactic_Elements_(Delphi))
- [FPC case statements](https://www.freepascal.org/docs-html/ref/refsu56.html)

Existing syntax-tree shapes and structured preprocessor support are retained.
Conditional expressions use `exprConditional` with `condition`, `then`, and `else`
fields. Compound operators retain separate keyword nodes so comments between their
words remain visible. Inline constants are not supported by this change. Inline
constants are deferred because the tested implementations
substantially increased error-recovery time on the large example file.

The grammar recognizes syntax, not compiler semantics such as constant-expression
evaluation, managed-record operator signatures, or type compatibility.

## Tree-sitter features:
- Syntax highlighting
- Scopes

## Screenshots

(using nvim-treesitter)

<a href=".doc/scr1.png"><img src=".doc/scr1.png" style="width: 22%; height: 22%"></a>
<a href=".doc/scr2.png"><img src=".doc/scr2.png" style="width: 22%; height: 22%"></a>
<a href=".doc/scr3.png"><img src=".doc/scr3.png" style="width: 22%; height: 22%"></a>
<a href=".doc/scr4.png"><img src=".doc/scr4.png" style="width: 22%; height: 22%"></a>
