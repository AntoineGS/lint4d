unit W1000Qualified;

interface

uses W1000QualifiedProvider;

procedure CallQualified;

implementation

procedure CallQualified;
begin
  W1000QualifiedProvider.DeprecatedRoutine;
end;

end.
