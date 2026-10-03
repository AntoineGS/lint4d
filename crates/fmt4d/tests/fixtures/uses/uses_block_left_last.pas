unit UsesBlockLeftLast;

interface

uses B, {$IFDEF X} C, {$ENDIF} A;

implementation

uses
  SysUtils,
  {$IFDEF X}
  MyApp.Extra,
  {$ENDIF}
  Classes;

end.
