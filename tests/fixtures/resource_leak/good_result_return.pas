unit GoodResultReturn;

interface

implementation

function CreateConnection: TFDConnection;
begin
  Result := TFDConnection.Create(nil);
  Result.ConnectionDefName := 'MyConn';
  Result.Connected := True;
end;

end.
