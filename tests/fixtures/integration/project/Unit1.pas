unit Unit1;

interface

type
  MyBadClass = class(TObject)
  public
    procedure DoWork;
  end;

implementation

procedure MyBadClass.DoWork;
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

  try
    WriteLn('risky');
  except
  end;

  with obj do
    WriteLn('scope ambiguity');
end;

end.
