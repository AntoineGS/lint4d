unit E2003MemberTypoRecord;

interface
type
  TSample = record
    Value: Integer;
  end;

implementation
procedure Run;
var Sample: TSample;
begin
  Sample.Misspelled := 1;
  Sample.Value := 2;
end;

end.
