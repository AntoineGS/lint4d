unit UsesTrailingBlockInBranch;

interface

uses {$IFDEF X} A {$IFDEF Y}, C {$ENDIF}; {$ELSE} D; {$ENDIF}

implementation

uses {$IFDEF X} A {$IFDEF Y}, C {$ENDIF} {$ELSE} D {$ENDIF};

end.
