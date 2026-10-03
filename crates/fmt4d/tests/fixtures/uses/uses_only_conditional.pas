unit UsesOnlyConditional;

interface

uses {$IFDEF X} A {$ELSE} B, C {$ENDIF};

implementation

uses A {$IFDEF X}, B{$ENDIF};

end.
