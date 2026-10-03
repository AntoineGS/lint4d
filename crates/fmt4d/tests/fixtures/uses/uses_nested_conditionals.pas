unit UsesNestedConditionals;

interface

uses Z, {$IFDEF X} C, {$ENDIF} {$IFDEF Y} D, {$IFDEF W} E, {$ENDIF} {$ENDIF} // tail
 A;

implementation

uses {$IFDEF X} {$IFDEF Y} A {$ELSE} B {$ENDIF}; {$ELSE} C; {$ENDIF}

end.
