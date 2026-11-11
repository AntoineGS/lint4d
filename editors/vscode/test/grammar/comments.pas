// SYNTAX TEST "source.pascal" "comments and directives"
x := 1; // trailing
//      ^^^^^^^^^^^ comment.line.double-slash.pascal
{ brace comment }
// <----------------- comment.block.brace.pascal
(* paren comment *)
// <------------------- comment.block.paren.pascal
{$IFDEF DEBUG}
// <-------------- meta.preprocessor.pascal
(*$R+*)
// <------- meta.preprocessor.pascal
