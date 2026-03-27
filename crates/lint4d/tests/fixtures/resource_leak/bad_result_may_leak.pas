unit BadResultMayLeak;

interface

implementation

function CreateBadObject: TObject;
begin
  Result := TObject.Create;

  if Result.ClassName <> 'somestring' then
    raise Exception.Create('test');
end;

end.
