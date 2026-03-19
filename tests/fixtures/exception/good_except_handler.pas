unit GoodExceptHandler;

interface

implementation

uses SysUtils;

procedure DoRisky;
begin
  try
    WriteLn('risky');
  except
    on E: Exception do
      WriteLn(E.Message);
  end;
end;

end.
