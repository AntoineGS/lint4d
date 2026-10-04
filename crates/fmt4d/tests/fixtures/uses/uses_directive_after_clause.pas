unit UsesDirectiveAfterClause;

interface

uses B, A; {$I a.inc}

implementation

uses
  {$IFDEF X}
  D;
  {$ELSE}
  C;
  {$ENDIF} {$I b.inc} // after the include

end.
