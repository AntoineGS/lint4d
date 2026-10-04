unit UsesCommaFirst;

interface

uses C, A {$IFDEF X}, B{$ENDIF};

implementation

uses Z, A {$IFDEF X}, B {$IFDEF Y}, C {$ENDIF} {$ENDIF} {$IFDEF W}, D{$ENDIF};

end.
