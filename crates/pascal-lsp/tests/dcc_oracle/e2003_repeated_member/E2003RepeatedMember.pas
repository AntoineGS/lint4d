unit E2003RepeatedMember;
interface
type TRec = record Known: Integer; end;
procedure Take(Value: Boolean);
implementation
procedure Run;
var B: Boolean; A: array[0..1] of Integer; R: TRec;
begin
  B := A[R.Missing];
  Take(A[R.Missing]);
end;
end.
