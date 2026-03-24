unit GoodResultTryExcept;

interface

implementation

function CreateSafeObject: TObject;
begin
  Result := TObject.Create;
  try
    if Result.ClassName <> 'somestring' then
      raise Exception.Create('test');
  except
    Result.Free;
    raise;
  end;
end;

end.
