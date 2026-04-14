unit T;

interface

implementation

procedure HandleError;
begin
  try
    RaiseSomething;
  except
    on E: {$IFDEF DELPHIXE2UP}System.{$ENDIF}SysUtils.Exception do
      WriteLn(E.Message);
  end;
end;

end.
