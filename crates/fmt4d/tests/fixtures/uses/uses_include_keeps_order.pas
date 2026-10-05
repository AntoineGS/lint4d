unit UsesIncludeKeepsOrder;

interface

uses B, A, {$I a.inc}, C {$I b.inc}, D;

implementation

uses
  {$IFDEF X}
  F {$I c.inc},
  {$ENDIF}
  E, {$R+} G;

end.
