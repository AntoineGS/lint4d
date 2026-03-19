unit BadUnprotected;

interface

implementation

procedure TestLeak;
var
  obj: TObject;
begin
  obj := TObject.Create;
  obj.ToString;
  try
    WriteLn('work');
  finally
    obj.Free;
  end;
end;

end.
