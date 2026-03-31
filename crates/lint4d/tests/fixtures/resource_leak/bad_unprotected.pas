unit BadUnprotected;

interface

implementation

procedure TestLeak;
var
  Obj: TObject;
begin
  Obj := TObject.Create;
  Obj.ToString;
  try
    WriteLn('work');
  finally
    Obj.Free;
  end;
end;

end.
