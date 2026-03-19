unit GoodNoWith;

interface

implementation

uses SysUtils;

procedure TestDirect;
var
  sl: TStringList;
begin
  sl := TStringList.Create;
  try
    sl.Add('hello');
  finally
    sl.Free;
  end;
end;

end.
