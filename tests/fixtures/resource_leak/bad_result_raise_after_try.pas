unit BadResultRaiseAfterTry;

interface

implementation

function CreateLeakyObject: TObject;
begin
  Result := TObject.Create;
  try
    DoSomething(Result);
  except
    Result.Free;
    raise;
  end;

  if Result.ClassName = 'bad' then
    raise Exception.Create('unprotected');
end;

end.
